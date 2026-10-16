//! A small reusable WYSIWYG rich-text editor: a `contentEditable` WebView with a
//! formatting toolbar. JavaScript runs only in this (our own) document, to drive
//! editing commands. Used for the compose body and the account signature.

use super::tool_flow::ToolFlow;
use adw::prelude::*;
use webkit6::prelude::WebViewExt;
use crate::i18n::{i18n, i18n_f, i18n_noop};

/// A rich-text editor widget. Add `widget` to a container; read the content back
/// with [`RichEditor::extract_html`] (asynchronous, since it queries the WebView).
/// Clones share the same underlying widgets (GObject references).
#[derive(Clone)]
pub struct RichEditor {
    /// The toolbar + editor, ready to be placed in a container.
    pub widget: gtk::Box,
    webview: webkit6::WebView,
    /// The formatting commands, hidden while a message is composed as plain
    /// text (#180) or as source — the row they sit in stays either way.
    toolbar_commands: ToolFlow,
    /// The far end of that row, where a host puts controls of its own (the
    /// composer's format chooser and preview toggle).
    toolbar_end: gtk::Box,
    /// Swaps the editor for the rendered preview of a source message.
    stack: gtk::Stack,
    /// The preview view, built the first time a preview is asked for: a
    /// second WebView per composer is not something to open speculatively
    /// (issue #106).
    preview: std::rc::Rc<std::cell::RefCell<Option<webkit6::WebView>>>,
    /// Which source language the editor currently holds, if it is in source
    /// mode rather than rich mode.
    source: std::rc::Rc<std::cell::Cell<Option<SourceKind>>>,
    /// Whether the rich document is showing the message as plain text
    /// (#424): flattened to what is sent, in the monospace font, taking
    /// only text. Read by every load of the document and by whatever puts
    /// files or the clipboard into it.
    plain: std::rc::Rc<std::cell::Cell<bool>>,
    /// Where "Send as Attachment Instead" delivers the lifted image, as a
    /// temp-file path the host adds to its attachment list. Set by the host
    /// via [`RichEditor::connect_send_as_attachment`].
    attach_cb: std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>>,
    /// Whether the document is there to take files, and the ones waiting
    /// for it ([`RichEditor::insert_files`]).
    loaded: std::rc::Rc<std::cell::Cell<bool>>,
    pending_files: std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>>,
    /// What the document's own text history can do right now, mirrored from
    /// WebKit's editor state so a host can read it without a round trip
    /// (#200 in the composer). `(undo, redo)`.
    text_history: std::rc::Rc<std::cell::Cell<(bool, bool)>>,
    /// Told whenever that pair changes, so a host can fold the body's text
    /// history into an undo stack of its own.
    history_cb: std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(bool, bool)>>>>,
    /// Where the right-click menu's Spelling Language choice goes, as the
    /// whole new setting (#365). The submenu is only offered once a host
    /// has connected it ([`RichEditor::connect_spell_languages`]).
    spell_langs_cb: SpellLangsCallback,
    /// The style manager's dark-notify handler that re-grounds the document
    /// on a live theme flip; disconnected when the last clone of the editor
    /// goes (the editor is a cloneable handle, so the guard is shared).
    _theme_handler: std::rc::Rc<ThemeHandlerGuard>,
    /// The Content-Security-Policy `<meta>` every document this editor loads
    /// carries, or empty when any picture may load. See [`RichEditor::new`].
    image_policy: std::rc::Rc<str>,
}

/// What a source-mode editor is holding. Rich text is the absence of
/// both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Markdown,
    Html,
}

type SpellLangsCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(String)>>>>;

/// Disconnects a style-manager handler on drop.
struct ThemeHandlerGuard(Option<gtk::glib::SignalHandlerId>);

impl Drop for ThemeHandlerGuard {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            adw::StyleManager::default().disconnect(id);
        }
    }
}

/// Push the spell-checking preference onto the shared web context (#114).
/// Checking only happens in editable views, so the reader — which shares the
/// context — is unaffected.
///
/// The languages are ALWAYS set when checking is on: enchant is given no
/// language at all until told, so "enabled" without this call underlines
/// nothing. The configured languages win; blank follows the session locale;
/// and each is swapped for the closest installed dictionary when its own
/// is missing, since a language without a dictionary silently checks
/// nothing. WebKit accepts a word any of the languages knows (#365).
pub fn apply_spellcheck() {
    let ctx = super::message_view::shared_web_context();
    let on = crate::config::load_privacy().spellcheck;
    ctx.set_spell_checking_enabled(on);
    if !on {
        return;
    }
    let langs = resolved_spell_languages();
    let langs: Vec<&str> = langs.iter().map(String::as_str).collect();
    ctx.set_spell_checking_languages(&langs);
}

/// The dictionaries checking actually runs with: the configured languages,
/// else the session locale, mapped onto the installed dictionaries.
pub fn resolved_spell_languages() -> Vec<String> {
    let configured = crate::config::load_privacy().spellcheck_langs;
    crate::spell::resolve_languages(&configured, locale_language(), &installed_dictionaries())
}

/// The dictionaries "System language" stands for: the session locale's.
pub fn system_spell_languages() -> Vec<String> {
    crate::spell::resolve_languages("", locale_language(), &installed_dictionaries())
}

/// The session locale as a dictionary-style code ("en_US.UTF-8" → "en_US").
fn locale_language() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|l| l.split('.').next().unwrap_or_default().to_string())
        .find(|l| !l.is_empty() && l != "C" && l != "POSIX")
}

/// The hunspell dictionaries that resolve to real files. Inside the Flatpak
/// most of `/usr/share/hunspell` is dangling symlinks until the locale
/// extension carries the language; `metadata()` follows symlinks, so a
/// dangling one errs out and is rightly skipped.
pub fn installed_dictionaries() -> Vec<String> {
    let mut codes: std::collections::BTreeSet<String> = Default::default();
    // Exactly the places enchant's hunspell provider reads (verified
    // empirically — a listed dictionary the checker can't use is the silent
    // failure this feature avoids): the classic locations, every SYSTEM XDG
    // data dir (which is how the Flatpak's bundled /app/share/hunspell set
    // is found), and enchant's per-user drop-in under the config dir. The
    // user DATA dir is deliberately absent: enchant does not search it.
    let mut dirs = vec![
        std::path::PathBuf::from("/usr/share/hunspell"),
        std::path::PathBuf::from("/usr/share/myspell"),
    ];
    for d in gtk::glib::system_data_dirs() {
        dirs.push(d.join("hunspell"));
    }
    dirs.push(gtk::glib::user_config_dir().join("enchant").join("hunspell"));
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            let is_dic = p.extension().is_some_and(|x| x == "dic");
            if is_dic && p.metadata().map(|m| m.is_file()).unwrap_or(false) {
                if let Some(s) = p.file_stem().and_then(|s| s.to_str()) {
                    codes.insert(s.to_string());
                }
            }
        }
    }
    // The runtime ships English many times over (eighteen region variants,
    // all hardlinks to the same data); the list keeps the five anyone looks
    // for. Other languages are shown as installed.
    const KEPT_ENGLISH: &[&str] = &["en_US", "en_GB", "en_CA", "en_AU", "en_NZ"];
    codes
        .into_iter()
        .filter(|c| {
            c.split('_').next() != Some("en") || KEPT_ENGLISH.contains(&c.as_str())
        })
        .collect()
}

/// The right-click menu's Spelling Language submenu (#365): the system
/// language, then a check item per installed dictionary, ticked as the
/// setting stands. Choosing one switches it as the matching switch in
/// Settings does, and `cb` takes the setting that results.
fn append_spelling_menu(menu: &webkit6::ContextMenu, cb: &SpellLangsCallback) {
    use gtk::prelude::ToVariant;
    if !crate::config::load_privacy().spellcheck {
        return;
    }
    let dicts = installed_dictionaries();
    if dicts.is_empty() {
        return;
    }
    let setting = crate::config::load_privacy().spellcheck_langs;
    let chosen = crate::spell::parse_languages(&setting);
    let system = system_spell_languages();
    let system_label = i18n_f(
        "System language — {language}",
        &[("language", &spell_language_names(&system))],
    );
    let entries = std::iter::once((String::new(), system_label, chosen.is_empty())).chain(
        dicts.iter().map(|d| (d.clone(), crate::spell::language_display_name(d), chosen.contains(d))),
    );
    let sub = webkit6::ContextMenu::new();
    for (i, (code, label, on)) in entries.enumerate() {
        // A boolean state is what makes WebKit draw a check item.
        let action = gtk::gio::SimpleAction::new_stateful(
            &format!("hylki-spell-lang-{i}"),
            None,
            &on.to_variant(),
        );
        let (cb, setting, system) = (cb.clone(), setting.clone(), system.clone());
        action.connect_activate(move |_, _| {
            let next = crate::spell::toggle_language(&setting, &code, !on, &system);
            if let Some(f) = cb.borrow().as_ref() {
                f(next);
            }
        });
        sub.append(&webkit6::ContextMenuItem::from_gaction(&action, &label, None));
        if i == 0 {
            sub.append(&webkit6::ContextMenuItem::new_separator());
        }
    }
    menu.append(&webkit6::ContextMenuItem::new_separator());
    menu.append(&webkit6::ContextMenuItem::with_submenu(&i18n("Spelling Language"), &sub));
}

/// Dictionary codes as the names people read, joined: "Deutsch
/// (Deutschland), Français (France)".
pub fn spell_language_names(codes: &[String]) -> String {
    codes.iter().map(|c| crate::spell::language_display_name(c)).collect::<Vec<_>>().join(", ")
}

impl RichEditor {
    /// `remote_images` is `None` when the editor may load any picture. A
    /// quoted original whose remote content the reader has not loaded gives
    /// the list the editor is limited to instead (the user's own signature
    /// pictures): opening a reply must not fetch what reading the message
    /// did not (#295). The markup keeps every address, so the recipient
    /// gets the quote whole.
    pub fn new(initial_html: &str, remote_images: Option<&[String]>, plain: bool) -> Self {
        let plain = std::rc::Rc::new(std::cell::Cell::new(plain));
        let image_policy: std::rc::Rc<str> = image_policy_meta(remote_images).into();
        // The shared document-viewer context (issue #106): a default-context
        // view would bring up a second web process with browser-sized caches.
        // Spell checking rides the same context; refreshing it here keeps a
        // just-changed preference honored without a restart.
        apply_spellcheck();
        // The message handler carries the word under the caret out for a
        // mid-word spelling verdict (see the paste script's caretWord).
        let ucm = webkit6::UserContentManager::new();
        ucm.register_script_message_handler("hylkiSpell", None);
        // The block state under the caret (quote, bulleted or numbered list)
        // comes out through this one, to light the matching toolbar button.
        ucm.register_script_message_handler("hylkiFormat", None);
        let webview = webkit6::WebView::builder()
            .web_context(&super::message_view::shared_web_context())
            .user_content_manager(&ucm)
            .build();
        crate::memory_report::register_web_view(&webview, "editor");
        // Everything the view itself owns (its content manager's handlers,
        // its event controllers, its toolbar's buttons via the format-state
        // handler) holds the view WEAKLY. A strong clone in any of those
        // closures is a reference cycle GObject never breaks: the editor of
        // a closed composer stayed alive that way, and with it its 300 MB
        // WebKitWebProcess, for the rest of the session (#221).
        {
            let weak = webview.downgrade();
            ucm.connect_script_message_received(Some("hylkiSpell"), move |_, value| {
                let Some(v) = weak.upgrade() else { return };
                let word = value.to_str().to_string();
                let bad = crate::spell::word_is_misspelled(&word);
                exec(&v, &format!("window.__hylkiSpellMark({bad})"));
            });
        }
        let settings = webkit6::Settings::new();
        settings.set_enable_javascript(true);
        settings.set_enable_developer_extras(false);
        // A script error in the editor document (all our own code) is
        // otherwise completely silent.
        settings.set_enable_write_console_messages_to_stdout(true);
        webview.set_settings(&settings);
        // Transparent until the document paints: a fresh WebView otherwise
        // renders an opaque default surface (black) for its first frames,
        // which flashes while the inline composer slides over the reader.
        // The document itself paints `Canvas` below, so the editor's normal
        // ground appears with the first real frame.
        webview.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
        // The document paints asynchronously: the host panel slides in
        // smoothly and the editor's content would land a beat later as a
        // pop. Start the view invisible and fade it in when its load
        // finishes — content arrives as a fade into the settled panel.
        webview.set_opacity(0.0);
        // Starting the editor's web process takes a moment, longest the
        // first time: past a glance, a spinner says it is coming (#392).
        let spinner = gtk::Spinner::new();
        spinner.set_size_request(32, 32);
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_can_target(false);
        spinner.set_visible(false);
        let shown = std::rc::Rc::new(std::cell::Cell::new(false));
        {
            let (sp, shown) = (spinner.downgrade(), shown.clone());
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                if let Some(sp) = sp.upgrade().filter(|_| !shown.get()) {
                    sp.set_visible(true);
                    sp.start();
                }
            });
        }
        let sp = spinner.downgrade();
        webview.connect_load_changed(move |v, ev| {
            if ev == webkit6::LoadEvent::Finished {
                shown.set(true);
                if let Some(sp) = sp.upgrade() {
                    sp.stop();
                    sp.set_visible(false);
                }
                fade_in(v);
                // Focus given before the document was there reached the
                // view but no caret: place it now.
                let root_focus = v.root().and_then(|r| r.focus());
                if root_focus.is_some_and(|f| f == *v.upcast_ref::<gtk::Widget>() || f.is_ancestor(v)) {
                    exec(v, PLACE_CARET);
                }
            }
        });
        {
            // Failsafe: whatever happens to the load, the editor must never
            // stay invisible.
            let (v, sp) = (webview.clone(), spinner.downgrade());
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
                if v.opacity() < 1.0 {
                    fade_in(&v);
                }
                if let Some(sp) = sp.upgrade() {
                    sp.stop();
                    sp.set_visible(false);
                }
            });
        }
        crate::web_fonts::load_html(
            &webview,
            &document(initial_html, &webview, &image_policy, plain.get()),
            Some("https://hylki.localhost/editor"),
        );
        // A live theme flip re-grounds the open document (#148): the scheme
        // and the ground are baked into the document at load, so without
        // this the editor stays in the scheme it was opened in. Deferred to
        // the next main-loop pass — the theme's named colors are only
        // re-resolved after the signal fires.
        let theme_handler = {
            let weak = webview.downgrade();
            adw::StyleManager::default().connect_dark_notify(move |sm| {
                reground(&weak, sm.is_dark());
            })
        };
        // An appearance theme moves the same colors with no scheme flip, so
        // the editor re-grounds for that too — and drops out of the list
        // once its view is gone.
        {
            let weak = webview.downgrade();
            crate::theme::connect_changed(move || {
                reground(&weak, adw::StyleManager::default().is_dark())
            });
        }

        // WebKitGTK never turns Ctrl+Z into an editing command: its key
        // binding translator forwards the key to a hidden GtkTextView and
        // listens for that widget's *signals*, and undo is a GtkTextView
        // action rather than a signal — so nothing comes back, and its own
        // custom table (bold, italic, tab, the newlines, Ctrl+Shift+V) has
        // no undo entry. The document's `execCommand('undo')` is refused
        // from script for the same reason it would be in a browser. Only
        // the widget API reaches the page's history, so the editor drives
        // it itself, and mirrors what it can do for the host to read.
        let text_history = std::rc::Rc::new(std::cell::Cell::new((false, false)));
        let history_cb: std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(bool, bool)>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        if let Some(state) = webview.editor_state() {
            let cell = text_history.clone();
            let cb = history_cb.clone();
            // Seeded before the first change so a host that asks early is
            // not told the document can undo when it cannot.
            cell.set((state.is_undo_available(), state.is_redo_available()));
            state.connect_changed(move |state| {
                let now = (state.is_undo_available(), state.is_redo_available());
                if cell.replace(now) == now {
                    return;
                }
                if let Some(f) = cb.borrow().as_ref() {
                    f(now.0, now.1);
                }
            });
        }
        {
            // Ctrl+Z, Ctrl+Shift+Z and Ctrl+Y over the body, in the capture
            // phase so they are decided before WebKit sees the key at all.
            // A host with a wider history of its own — the composer, whose
            // undo also covers its attachments — catches them further up,
            // where capture reaches it first, and this never runs.
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = webview.downgrade();
            keys.connect_key_pressed(move |_, keyval, _, state| {
                let Some(v) = weak.upgrade() else { return gtk::glib::Propagation::Proceed };
                match history_key(keyval, state) {
                    Some(redo) => {
                        v.execute_editing_command(if redo {
                            webkit6::EDITING_COMMAND_REDO
                        } else {
                            webkit6::EDITING_COMMAND_UNDO
                        });
                        gtk::glib::Propagation::Stop
                    }
                    None => gtk::glib::Propagation::Proceed,
                }
            });
            webview.add_controller(keys);
        }

        // The stock editable menu's single "Paste" hides the plain/rich choice
        // behind the preference; the menu offers both, always, in its place.
        // Over an image the menu leads with image actions — cut, copy, and
        // demoting an inline picture to an ordinary attachment — acting on
        // the exact node the right-click landed on (`__hylkiCtxImg`).
        let attach_cb: std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        // Files to put in the text that arrived before the document did (a
        // message started from files dropped on the main window): they go
        // in once it has loaded, and not into the document being replaced.
        let loaded = std::rc::Rc::new(std::cell::Cell::new(false));
        let pending_files: std::rc::Rc<std::cell::RefCell<Vec<std::path::PathBuf>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        {
            let (loaded, pending, cb) = (loaded.clone(), pending_files.clone(), attach_cb.clone());
            let plain = plain.clone();
            webview.connect_load_changed(move |v, ev| match ev {
                webkit6::LoadEvent::Started => loaded.set(false),
                webkit6::LoadEvent::Finished => {
                    loaded.set(true);
                    let paths = std::mem::take(&mut *pending.borrow_mut());
                    if !paths.is_empty() {
                        let files: Vec<_> = paths.iter().map(gtk::gio::File::for_path).collect();
                        deliver_files(v, &files, None, &cb, !plain.get());
                    }
                }
                _ => {}
            });
        }
        // Rich to begin with; [`RichEditor::set_source`] moves it.
        let source: std::rc::Rc<std::cell::Cell<Option<SourceKind>>> =
            std::rc::Rc::new(std::cell::Cell::new(None));
        let menu_attach_cb = attach_cb.clone();
        let menu_history = text_history.clone();
        let menu_plain = plain.clone();
        let spell_langs_cb: SpellLangsCallback = Default::default();
        let menu_spell_cb = spell_langs_cb.clone();
        webview.connect_context_menu(move |view, menu, hit| {
            crate::ui::message_view::strip_navigation_items(menu);
            if hit.context_is_image() {
                // Stock image entries (copy/save/open variants) are replaced
                // by ours, which also know about the editable document.
                for item in menu.items().iter().filter(|i| {
                    matches!(
                        i.stock_action(),
                        webkit6::ContextMenuAction::CopyImageToClipboard
                            | webkit6::ContextMenuAction::CopyImageUrlToClipboard
                            | webkit6::ContextMenuAction::DownloadImageToDisk
                            | webkit6::ContextMenuAction::OpenImageInNewWindow
                    )
                }) {
                    menu.remove(item);
                }
                // The stock Cut/Copy already act on the image — the
                // right-click selected it. The one image-specific verb is
                // demoting it to an ordinary attachment; the script that
                // lifts it declines non-embedded (remote) images itself,
                // since the hit test doesn't surface data: URIs to check
                // against here.
                let action = gtk::gio::SimpleAction::new("hylki-img-attach", None);
                let v = view.clone();
                let cb = menu_attach_cb.clone();
                action.connect_activate(move |_, _| detach_ctx_image(&v, cb.clone()));
                menu.insert(
                    &webkit6::ContextMenuItem::from_gaction(
                        &action,
                        &i18n("Send as Attachment Instead"),
                        None,
                    ),
                    0,
                );
                menu.insert(&webkit6::ContextMenuItem::new_separator(), 1);
                // Sizes lead the menu: the fractions are of the writing
                // width, which is the width the recipient reads at, so
                // "Large" means the full column rather than some pixel count
                // that means nothing on the other end. Dragging the corner
                // handles does the same thing by hand.
                let mut at = 0;
                for (label, kind) in [
                    (i18n("Small"), "small"),
                    (i18n("Medium"), "medium"),
                    (i18n("Large"), "large"),
                    (i18n("Original Size"), "original"),
                ] {
                    let action =
                        gtk::gio::SimpleAction::new(&format!("hylki-img-{kind}"), None);
                    let v = view.clone();
                    action.connect_activate(move |_, _| {
                        exec(&v, &format!("window.__hylkiSetImageSize('{kind}')"));
                    });
                    menu.insert(
                        &webkit6::ContextMenuItem::from_gaction(&action, &label, None),
                        at,
                    );
                    at += 1;
                }
                {
                    let action = gtk::gio::SimpleAction::new("hylki-img-fit", None);
                    let v = view.clone();
                    let hint = js_escape(&i18n(
                        "Will be recompressed to the size shown when this is sent",
                    ));
                    action.connect_activate(move |_, _| {
                        exec(&v, &format!("window.__hylkiToggleFit('{hint}')"));
                    });
                    menu.insert(
                        &webkit6::ContextMenuItem::from_gaction(
                            &action,
                            &i18n("Recompress to This Size on Send"),
                            None,
                        ),
                        at,
                    );
                    at += 1;
                }
                menu.insert(&webkit6::ContextMenuItem::new_separator(), at);
            }
            // Which dictionaries check the message, switched one at a time
            // without a trip to Settings (#365). It is the app-wide setting,
            // so every open composer follows.
            if !hit.context_is_image() && menu_spell_cb.borrow().is_some() {
                append_spelling_menu(menu, &menu_spell_cb);
            }
            let items = menu.items();
            let Some(pos) = items
                .iter()
                .position(|i| i.stock_action() == webkit6::ContextMenuAction::Paste)
            else {
                return false; // no paste here (nothing editable hit): stock menu as-is
            };
            for item in items.iter().filter(|i| {
                matches!(
                    i.stock_action(),
                    webkit6::ContextMenuAction::Paste | webkit6::ContextMenuAction::PasteAsPlainText
                )
            }) {
                menu.remove(item);
            }
            let mut at = pos as i32;
            // Plain text has no formatting to keep, so it offers only the
            // paste it does.
            let plain = menu_plain.get();
            for (label, rich) in [(i18n("Paste with Formatting"), true), (i18n("Paste as Plain Text"), false)] {
                if rich && plain {
                    continue;
                }
                let action =
                    gtk::gio::SimpleAction::new(if rich { "hylki-paste-rich" } else { "hylki-paste-plain" }, None);
                let v = view.clone();
                let cb = menu_attach_cb.clone();
                action.connect_activate(move |_, _| paste_into(&v, rich, &cb, plain));
                menu.insert(&webkit6::ContextMenuItem::from_gaction(&action, &label, None), at);
                at += 1;
            }
            // Undo and Redo lead an editable menu, as they do everywhere
            // else — the one place the body's text history is visible at
            // all, rather than only on the keys. Inserted last so the
            // paste group's index above is still the one it was measured
            // at. Over an image the sizes keep the top: that menu is about
            // the picture under the pointer, not the message.
            if !hit.context_is_image() {
                let (can_undo, can_redo) = menu_history.get();
                for (label, name, redo, on) in [
                    (i18n("Undo"), "hylki-undo", false, can_undo),
                    (i18n("Redo"), "hylki-redo", true, can_redo),
                ] {
                    let action = gtk::gio::SimpleAction::new(name, None);
                    action.set_enabled(on);
                    let v = view.clone();
                    action.connect_activate(move |_, _| {
                        v.execute_editing_command(if redo {
                            webkit6::EDITING_COMMAND_REDO
                        } else {
                            webkit6::EDITING_COMMAND_UNDO
                        });
                    });
                    menu.insert(
                        &webkit6::ContextMenuItem::from_gaction(&action, &label, None),
                        i32::from(redo),
                    );
                }
                menu.insert(&webkit6::ContextMenuItem::new_separator(), 2);
            }
            false
        });

        // Files dropped from a file manager never reach the document: WebKitGTK
        // hands the page a uri-list it then refuses to serve, so the drop
        // handler in PASTE_SCRIPT sees an empty `DataTransfer.files` and lets
        // WebKit insert the path as a link. Only the widget can see them, so
        // the widget takes them — declaring just `GdkFileList` leaves every
        // other drag (text, an image dragged out of the reader) to WebKit.
        {
            let drop =
                gtk::DropTarget::new(gtk::gdk::FileList::static_type(), gtk::gdk::DragAction::COPY);
            drop.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = webview.downgrade();
            let cb = attach_cb.clone();
            let src = source.clone();
            let plain = plain.clone();
            drop.connect_drop(move |_, value, x, y| {
                let Some(v) = weak.upgrade() else { return false };
                let Ok(list) = value.get::<gtk::gdk::FileList>() else { return false };
                // Source mode has no document to hold a picture: everything
                // dropped becomes an attachment, and the user writes the
                // reference to it themselves.
                if src.get().is_some() {
                    return attach_all(&list.files(), &cb);
                }
                deliver_files(&v, &list.files(), Some((x, y)), &cb, !plain.get())
            });
            webview.add_controller(drop);
        }

        // The editor and (later) its preview are pages of one stack, so a
        // preview swaps in without the editor document being torn down and
        // rebuilt — which would cost the undo history and the caret.
        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        let edit = gtk::Overlay::new();
        edit.set_child(Some(&webview));
        edit.add_overlay(&spinner);
        stack.add_named(&edit, Some("edit"));

        let expanded = crate::config::load_privacy().toolbar_expanded;
        let (toolbar, toolbar_commands, toolbar_end, bar_state) = build_toolbar(&webview, &stack, &attach_cb, expanded);
        // `fmtState` in PASTE_SCRIPT posts where the caret is: the block
        // kinds it sits in (`q` quote, `u` bulleted list, `o` numbered
        // list), its paragraph style and its font. A set toggle also means a
        // click on it leaves that block instead of opening another (issue
        // #137, EmmanuelP's follow-up).
        ucm.connect_script_message_received(Some("hylkiFormat"), move |_, value| {
            bar_state.update(&value.to_str());
        });

        let frame = gtk::Frame::new(None);
        frame.set_vexpand(true);
        // Same card treatment as the address fields' boxed-list: shadow and
        // radius from libadwaita, with the WebView clipped to the corners.
        frame.add_css_class("card");
        // Found by name when a sent reply's editor turns into its card in
        // the conversation (send_morph.rs).
        frame.add_css_class("compose-editor-card");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_child(Some(&stack));

        {
            // Ctrl+. and Ctrl+; open WebKitGTK's own emoji chooser, but
            // what is picked in it never reaches the page (#348; WebKitGTK
            // 2.50 to 2.54 at least, a bare WebView does the same). The
            // editor opens GTK's chooser itself, at the caret, and types
            // the emoji in.
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            let weak = webview.downgrade();
            let anchor = stack.downgrade();
            keys.connect_key_pressed(move |_, keyval, _, state| {
                use gtk::gdk::{Key, ModifierType};
                let mods = state & (ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK | ModifierType::ALT_MASK);
                if mods != ModifierType::CONTROL_MASK || !matches!(keyval, Key::period | Key::semicolon) {
                    return gtk::glib::Propagation::Proceed;
                }
                let (Some(v), Some(anchor)) = (weak.upgrade(), anchor.upgrade()) else {
                    return gtk::glib::Propagation::Proceed;
                };
                show_emoji_chooser(&v, &anchor);
                gtk::glib::Propagation::Stop
            });
            webview.add_controller(keys);
        }

        let bx = gtk::Box::new(gtk::Orientation::Vertical, 6);
        bx.append(&toolbar);
        bx.append(&frame);

        RichEditor {
            widget: bx,
            webview,
            toolbar_commands,
            toolbar_end,
            stack,
            preview: std::rc::Rc::new(std::cell::RefCell::new(None)),
            source: source.clone(),
            plain,
            attach_cb,
            loaded,
            pending_files,
            text_history,
            history_cb,
            spell_langs_cb,
            _theme_handler: std::rc::Rc::new(ThemeHandlerGuard(Some(theme_handler))),
            image_policy,
        }
    }

    /// Step the document's own text history back (`redo` false) or forward.
    /// Asynchronous like every other page command: [`RichEditor::can_undo`]
    /// only catches up when WebKit reports the new state.
    pub fn undo(&self) {
        self.webview.execute_editing_command(webkit6::EDITING_COMMAND_UNDO);
    }

    pub fn redo(&self) {
        self.webview.execute_editing_command(webkit6::EDITING_COMMAND_REDO);
    }

    /// Run any WebCore editing command against the document ("DeleteBackward",
    /// "InsertParagraph", …). The widget API is the only route to them that
    /// reaches the page's own history — the same reason [`RichEditor::undo`]
    /// cannot go through `execCommand`.
    pub fn editing_command(&self, command: &str) {
        self.webview.execute_editing_command(command);
    }

    /// Whether the body has anything left to take back. Read from the
    /// mirrored editor state, so it is a main-loop pass or two behind a
    /// command just issued — a host must treat "still true" right after
    /// [`RichEditor::undo`] as "ask again", not as a miscount.
    pub fn can_undo(&self) -> bool {
        self.text_history.get().0
    }

    pub fn can_redo(&self) -> bool {
        self.text_history.get().1
    }

    /// Called with `(can_undo, can_redo)` whenever the body's text history
    /// changes: an edit, a command, or a reload that drops it.
    pub fn connect_history_changed(&self, f: impl Fn(bool, bool) + 'static) {
        *self.history_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Offer the Spelling Language submenu, and receive the setting it
    /// makes: the comma-separated language list, empty for the system
    /// language (#365).
    pub fn connect_spell_languages(&self, f: impl Fn(String) + 'static) {
        *self.spell_langs_cb.borrow_mut() = Some(Box::new(f));
    }

    /// What "Send as Attachment Instead" does with the lifted image: the
    /// host receives the temp file's path and adds it to its attachments.
    pub fn connect_send_as_attachment(&self, f: impl Fn(std::path::PathBuf) + 'static) {
        *self.attach_cb.borrow_mut() = Some(Box::new(f));
    }

    /// Replace the editor contents with `content` (HTML), in rich mode, or
    /// flattened to plain text when [`RichEditor::set_plain`] says so.
    pub fn set_html(&self, content: &str) {
        self.source.set(None);
        self.show_preview(None);
        crate::web_fonts::load_html(
            &self.webview,
            &document(content, &self.webview, &self.image_policy, self.plain.get()),
            Some("https://hylki.localhost/editor"),
        );
    }

    /// Put the editor into source mode holding `text`: a monospace
    /// plain-text surface with no formatting toolbar, which the composer
    /// converts on the way out.
    pub fn set_source(&self, kind: SourceKind, text: &str) {
        self.source.set(Some(kind));
        self.show_preview(None);
        crate::web_fonts::load_html(
            &self.webview,
            &source_document(text, &self.webview),
            Some("https://hylki.localhost/editor"),
        );
    }

    /// Show the message as plain text from the next [`RichEditor::set_html`]
    /// on: the document flattens itself to the text that is sent, in the
    /// monospace font, and pastes and drops bring in text alone (#424).
    pub fn set_plain(&self, on: bool) {
        self.plain.set(on);
    }

    /// The source language the editor is in, or `None` in rich mode.
    pub fn source_kind(&self) -> Option<SourceKind> {
        self.source.get()
    }

    /// Read the source text back, exactly as typed.
    pub fn extract_source(&self, cb: impl FnOnce(String) + 'static) {
        self.webview.evaluate_javascript(
            "window.__hylkiSourceText ? window.__hylkiSourceText() : ''",
            None,
            None,
            gtk::gio::Cancellable::NONE,
            move |res| cb(res.map(|v| v.to_str().to_string()).unwrap_or_default()),
        );
    }

    /// Show `html` rendered in place of the editor, or go back to editing.
    /// The preview is read-only and runs no scripts — it is a look at the
    /// message, not a second editor.
    pub fn show_preview(&self, html: Option<&str>) {
        let Some(html) = html else {
            self.stack.set_visible_child_name("edit");
            return;
        };
        // Taken out first: a `match` on `borrow()` holds that borrow for the
        // whole match, and the arm below needs `borrow_mut()`.
        let existing = self.preview.borrow().clone();
        let view = match existing {
            Some(v) => v,
            None => {
                let settings = webkit6::Settings::new();
                settings.set_enable_javascript(false);
                settings.set_enable_developer_extras(false);
                let v = webkit6::WebView::builder()
                    .web_context(&super::message_view::shared_web_context())
                    .settings(&settings)
                    .build();
                crate::memory_report::register_web_view(&v, "editor preview");
                v.set_background_color(&gtk::gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
                self.stack.add_named(&v, Some("preview"));
                *self.preview.borrow_mut() = Some(v.clone());
                v
            }
        };
        crate::web_fonts::load_html(
            &view,
            &preview_document(html, &view, &self.image_policy),
            Some("https://hylki.localhost/preview"),
        );
        self.stack.set_visible_child_name("preview");
    }

    pub fn grab_focus(&self) {
        self.webview.grab_focus();
        if self.source.get().is_some() {
            // The document's own field is what takes the caret.
            exec(&self.webview, "var t=document.getElementById('src');if(t)t.focus();");
        } else {
            exec(&self.webview, PLACE_CARET);
        }
    }

    /// Run a JavaScript snippet against the editor document (e.g. to swap the
    /// signature block when the From account changes).
    pub fn run_js(&self, js: &str) {
        exec(&self.webview, js);
    }

    /// Run a snippet and hand back what it returns, as text ("" when it
    /// failed).
    pub fn eval(&self, js: &str, cb: impl FnOnce(String) + 'static) {
        self.webview.evaluate_javascript(js, None, None, gtk::gio::Cancellable::NONE, move |res| {
            cb(res.map(|v| v.to_str().to_string()).unwrap_or_default())
        });
    }

    /// Whether keyboard focus is currently inside the editor's WebView — the
    /// guard a host's paste shortcut uses so it never hijacks Ctrl+V aimed at
    /// an address entry.
    pub fn has_focus(&self) -> bool {
        self.webview
            .root()
            .and_then(|r| r.focus())
            .is_some_and(|f| f == *self.webview.upcast_ref::<gtk::Widget>() || f.is_ancestor(&self.webview))
    }

    /// Paste the clipboard into the editor, keeping (`rich`) or stripping the
    /// clipboard's formatting for this one paste, whatever the standing
    /// preference says.
    /// Put the pictures among `paths` in the text at the caret, and hand
    /// the rest to the attachment list, as a drop on the text does. Before
    /// the document has loaded they wait for it.
    pub fn insert_files(&self, paths: &[std::path::PathBuf]) {
        if !self.loaded.get() {
            self.pending_files.borrow_mut().extend(paths.iter().cloned());
            return;
        }
        let files: Vec<_> = paths.iter().map(gtk::gio::File::for_path).collect();
        deliver_files(&self.webview, &files, None, &self.attach_cb, !self.plain.get());
    }

    pub fn paste(&self, rich: bool) {
        paste_into(&self.webview, rich, &self.attach_cb, self.plain.get());
    }

    /// Whether the body has been edited since it was loaded (async read of a JS
    /// flag set on the first `input` event). Used to avoid saving a pristine,
    /// quote-only reply to Drafts when the reader navigates away.
    pub fn is_dirty(&self, cb: impl FnOnce(bool) + 'static) {
        self.webview.evaluate_javascript(
            "String(!!window.__hylkiDirty)",
            None,
            None,
            gtk::gio::Cancellable::NONE,
            move |res| cb(res.map(|v| v.to_str() == "true").unwrap_or(false)),
        );
    }

    /// Show or hide the formatting commands: hidden while the message is
    /// composed as plain text (#180) or as source, where they would go
    /// nowhere. The row itself stays, carrying whatever the host put at its
    /// end — a chooser that vanished with the buttons would be the one
    /// control needed to get them back.
    pub fn set_formatting_visible(&self, on: bool) {
        self.toolbar_commands.set_tools_shown(on);
    }

    /// The far end of the formatting row, for a host's own controls.
    /// Keep the text area at least `height` pixels tall, whatever the
    /// toolbar above it takes: a host with a fixed-size slot would
    /// otherwise lose text to a wrapped or expanded toolbar.
    pub fn set_text_height(&self, height: i32) {
        self.stack.set_height_request(height);
    }

    pub fn toolbar_end(&self) -> &gtk::Box {
        &self.toolbar_end
    }

    /// Read the current body HTML asynchronously.
    pub fn extract_html(&self, cb: impl FnOnce(String) + 'static) {
        self.webview.evaluate_javascript(
            "window.__hylkiBodyHtml()",
            None,
            None,
            gtk::gio::Cancellable::NONE,
            move |res| cb(res.map(|v| v.to_str().to_string()).unwrap_or_default()),
        );
    }

    /// Read the body for *sending*: any picture armed with "Recompress to
    /// This Size on Send" is recut to the size it is drawn at first. Drafts
    /// go through [`RichEditor::extract`], which never touches the pixels —
    /// a save must not cost quality.
    pub fn extract_for_send(&self, cb: impl FnOnce(String, String) + 'static) {
        self.read_body("window.__hylkiBodyHtmlForSend()", cb);
    }

    /// Read the current body HTML and a plain-text rendering asynchronously.
    pub fn extract(&self, cb: impl FnOnce(String, String) + 'static) {
        self.read_body("window.__hylkiBodyHtml()", cb);
    }

    fn read_body(&self, reader: &str, cb: impl FnOnce(String, String) + 'static) {
        // JSON, not a separator: the value comes back as a C string, so a NUL
        // between the halves cut the text off and plain-text mail went out
        // empty (#297).
        self.webview.evaluate_javascript(
            &format!("JSON.stringify([{reader}, window.__hylkiBodyText()])"),
            None,
            None,
            gtk::gio::Cancellable::NONE,
            move |res| {
                let json = res.map(|v| v.to_str().to_string()).unwrap_or_default();
                let (html, text) = split_body(&json);
                cb(html, text);
            },
        );
    }
}

/// The HTML and the text of the body from `read_body`'s JSON pair.
fn split_body(json: &str) -> (String, String) {
    serde_json::from_str::<(String, String)>(json).unwrap_or_default()
}

/// Whether a keystroke is an undo (`Some(false)`) or a redo (`Some(true)`).
/// Ctrl+Y is bound alongside Ctrl+Shift+Z because both spellings are in wide
/// use and the app's own history (#200) already answers to each.
pub fn history_key(keyval: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> Option<bool> {
    if !state.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
        return None;
    }
    let shift = state.contains(gtk::gdk::ModifierType::SHIFT_MASK);
    if !shift && keyval == gtk::gdk::Key::z {
        return Some(false);
    }
    if (shift && matches!(keyval, gtk::gdk::Key::z | gtk::gdk::Key::Z))
        || keyval == gtk::gdk::Key::y
    {
        return Some(true);
    }
    None
}

/// Give the rich document's body the caret. Keyboard focus on the view
/// alone left no caret in it, so a reply opened with nowhere to type
/// until the body was clicked (#266). A caret already in the body stays
/// where it is; otherwise it goes to the top, above any quote. Only a
/// contenteditable body is touched: the source document's field takes
/// focus itself.
const PLACE_CARET: &str = "(function(){var b=document.body;\
    if(!b||!b.isContentEditable)return;\
    var s=getSelection();\
    var kept=s.rangeCount&&b.contains(s.anchorNode)?s.getRangeAt(0).cloneRange():null;\
    b.focus();\
    var r=kept;\
    if(!r){r=document.createRange();r.setStart(b,0);r.collapse(true);}\
    s.removeAllRanges();s.addRange(r);})()";

/// GTK's emoji chooser over the editor, pointing at the caret (#348). It is
/// parented only while it is open, so a closed composer leaves nothing
/// behind.
fn show_emoji_chooser(webview: &webkit6::WebView, anchor: &gtk::Stack) {
    let weak = webview.downgrade();
    let anchor = anchor.downgrade();
    webview.evaluate_javascript(
        "window.__hylkiEmojiMark ? window.__hylkiEmojiMark() : ''",
        None,
        None,
        gtk::gio::Cancellable::NONE,
        move |res| {
            let (Some(v), Some(anchor)) = (weak.upgrade(), anchor.upgrade()) else { return };
            let chooser = gtk::EmojiChooser::new();
            chooser.set_parent(&anchor);
            let at: Vec<f64> = res
                .map(|r| r.to_str().to_string())
                .unwrap_or_default()
                .split(',')
                .filter_map(|n| n.trim().parse().ok())
                .collect();
            if let [x, y, w, h] = at[..] {
                // CSS pixels are widget pixels times the zoom.
                let z = v.zoom_level();
                let (x, y) = v
                    .compute_point(&anchor, &gtk::graphene::Point::new((x * z) as f32, (y * z) as f32))
                    .map_or((x * z, y * z), |p| (p.x() as f64, p.y() as f64));
                chooser.set_pointing_to(Some(&gtk::gdk::Rectangle::new(
                    x as i32,
                    y as i32,
                    (w * z).max(1.0) as i32,
                    (h * z).max(1.0) as i32,
                )));
            }
            let weak = v.downgrade();
            chooser.connect_emoji_picked(move |_, emoji| {
                let Some(v) = weak.upgrade() else { return };
                v.grab_focus();
                let text = serde_json::to_string(emoji).unwrap_or_default();
                exec(&v, &format!("window.__hylkiEmojiPut && window.__hylkiEmojiPut({text})"));
            });
            chooser.connect_closed(|c| {
                // Unparented after the pick has been handled, not inside the
                // signal that is still running on it.
                let c = c.clone();
                gtk::glib::idle_add_local_once(move || c.unparent());
            });
            chooser.popup();
        },
    );
}

fn exec(webview: &webkit6::WebView, js: &str) {
    webview.evaluate_javascript(js, None, None, gtk::gio::Cancellable::NONE, |_| {});
}

/// Fade the editor view to full opacity. The animation object is parked on
/// the view itself so it stays alive for its 200ms (a dropped animation
/// stops); a repeat call replaces the parked one harmlessly.
fn fade_in(webview: &webkit6::WebView) {
    let anim = adw::TimedAnimation::new(
        webview,
        webview.opacity(),
        1.0,
        200,
        adw::CallbackAnimationTarget::new({
            let v = webview.clone();
            move |o| v.set_opacity(o)
        }),
    );
    anim.set_easing(adw::Easing::EaseOutCubic);
    anim.play();
    unsafe {
        webview.set_data("hylki-fade-anim", anim);
    }
}

/// "Send as Attachment Instead": pull the right-clicked image out of the
/// document, decode its data: URI to a temp file, and hand the path to the
/// host's callback (which adds it to the attachment list). The node is
/// removed in the same script that reads it, so the demotion is atomic even
/// if the same picture was pasted twice.
fn detach_ctx_image(
    webview: &webkit6::WebView,
    cb: std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>>,
) {
    webview.evaluate_javascript(
        "(function(){var n=window.__hylkiCtxImg;\
          if(!n){var sel=getSelection();\
            if(sel.rangeCount===1){var r=sel.getRangeAt(0);\
              if(r.startContainer===r.endContainer&&r.endOffset-r.startOffset===1){\
                var c=r.startContainer.childNodes[r.startOffset];\
                if(c&&c.tagName==='IMG')n=c;}}}\
          if(!n||n.tagName!=='IMG')return 'E:none';\
          var s=n.src;if(s.indexOf('data:image/')!==0)return 'E:src:'+s.slice(0,40);\
          n.remove();window.__hylkiCtxImg=null;return s;})()",
        None,
        None,
        gtk::gio::Cancellable::NONE,
        move |res| {
            let v = match res {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("img-attach: script failed: {e}");
                    return;
                }
            };
            let src = v.to_str();
            if let Some(err) = src.strip_prefix("E:") {
                tracing::warn!("img-attach: script declined: {err}");
                return;
            }
            let Some(path) = data_uri_to_temp_file(&src) else {
                tracing::warn!("img-attach: not a liftable data: image");
                return;
            };
            match cb.borrow().as_ref() {
                Some(f) => f(path),
                None => tracing::warn!("img-attach: no attach callback set on this editor"),
            }
        },
    );
}

/// Write a `data:image/...;base64,...` URI's bytes to a temp file the send
/// path can read like any chosen attachment. The name it gets is the name
/// that goes out on the wire.
fn data_uri_to_temp_file(uri: &str) -> Option<std::path::PathBuf> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?;
    if !mime.starts_with("image/") {
        return None;
    }
    let data = crate::oauth::base64_decode(payload)?;
    let ext = match mime {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "png",
    };
    let dir = std::env::temp_dir().join("vireo-inline-images");
    std::fs::create_dir_all(&dir).ok()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("image-{stamp}.{ext}"));
    std::fs::write(&path, &data).ok()?;
    Some(path)
}

/// One paste in an explicit mode: arm the document's one-shot flag, then run
/// the editing command once the flag is in place (the callback orders the two,
/// since both travel to the web process asynchronously). The DOM paste handler
/// consumes the flag — see [`PASTE_SCRIPT`].
///
/// A file copied in a file manager is taken first, for the same reason the
/// drop target exists: the clipboard offers `GdkFileList`, but by the time
/// WebKit has turned it into a paste the document sees an empty
/// `clipboardData` and the bare path arrives as text. The formats are checked
/// synchronously so an ordinary text paste is not delayed by a read that was
/// always going to fail.
fn paste_into(
    webview: &webkit6::WebView,
    rich: bool,
    cb: &std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>>,
    plain: bool,
) {
    if webview.clipboard().formats().contains_type(gtk::gdk::FileList::static_type()) {
        let v = webview.clone();
        let cb = cb.clone();
        webview.clipboard().read_value_async(
            gtk::gdk::FileList::static_type(),
            gtk::glib::Priority::DEFAULT,
            gtk::gio::Cancellable::NONE,
            move |res| {
                let taken = res
                    .ok()
                    .and_then(|val| val.get::<gtk::gdk::FileList>().ok())
                    .is_some_and(|list| deliver_files(&v, &list.files(), None, &cb, !plain));
                // Nothing usable on the clipboard after all: the ordinary
                // paste still owes the user whatever else is on it.
                if !taken {
                    paste_via_webkit(&v, rich);
                }
            },
        );
        return;
    }
    paste_via_webkit(webview, rich);
}

/// The stock paste: arm the one-shot mode flag, then let WebKit paste.
fn paste_via_webkit(webview: &webkit6::WebView, rich: bool) {
    let v = webview.clone();
    webview.evaluate_javascript(
        &format!("window.__hylkiPasteOnce={rich};"),
        None,
        None,
        gtk::gio::Cancellable::NONE,
        move |_| v.execute_editing_command("Paste"),
    );
}

/// Images inline, everything else attached. `at` is the drop point in widget
/// coordinates (which are the document's viewport coordinates), placing the
/// caret where the picture was let go; a paste keeps the caret it has.
/// Returns whether any file was taken, so a caller can fall back. With
/// `inline` off (a message written as plain text) every file is attached.
///
/// Reading and encoding happen here rather than in the document because the
/// document is never given the file in the first place — see [`paste_into`].
fn deliver_files(
    webview: &webkit6::WebView,
    files: &[gtk::gio::File],
    at: Option<(f64, f64)>,
    cb: &std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>>,
    inline: bool,
) -> bool {
    if !inline {
        return attach_all(files, cb);
    }
    let mut took = false;
    let mut point = at;
    for file in files {
        let Some(path) = file.path() else { continue };
        match read_image_for_insert(file, &path) {
            Some((b64, mime)) => {
                let (x, y) = point.take().map_or(("null".into(), "null".into()), |(x, y)| {
                    (format!("{x}"), format!("{y}"))
                });
                // The base64 goes in unescaped on purpose: its alphabet has
                // nothing a JS string literal cares about, and running the
                // escape over tens of megabytes of it costs real time. Only
                // the MIME type comes from outside.
                let mime = js_escape(&mime);
                let name = js_escape(
                    &path.file_name().unwrap_or_default().to_string_lossy(),
                );
                exec(
                    webview,
                    &format!(
                        "window.__hylkiInsertImageURL(\
                         'data:{mime};base64,{b64}','{mime}','{name}',{x},{y})"
                    ),
                );
                took = true;
            }
            // Not a picture, or too big to carry in the body: the composer's
            // attachment list is where it belongs. Inline images are lifted
            // into `cid:` parts at send time either way, so an attachment is
            // the same journey with a different disposition.
            None => match cb.borrow().as_ref() {
                Some(f) => {
                    f(path);
                    took = true;
                }
                None => tracing::warn!("editor drop: no attach callback set on this editor"),
            },
        }
    }
    took
}

/// An image small enough to inline, as base64 of its bytes plus its MIME type.
/// Anything else — a document, an unreadable file, or a picture so large that
/// base64 of it would be a burden to ferry into the web process — returns
/// `None` and becomes an attachment instead. The cap is deliberately generous:
/// the document downscales to 1600px on insert, so the size that matters to
/// the recipient is decided there, not here.
fn read_image_for_insert(
    file: &gtk::gio::File,
    path: &std::path::Path,
) -> Option<(String, String)> {
    let mime = inline_mime(file)?;
    let data = std::fs::read(path).ok()?;
    Some((crate::oauth::base64_encode(&data), mime))
}

/// The MIME type of a picture that can go in the text, or `None`.
fn inline_mime(file: &gtk::gio::File) -> Option<String> {
    const MAX_INLINE_BYTES: u64 = 32 * 1024 * 1024;
    let info = file
        .query_info(
            "standard::content-type,standard::size",
            gtk::gio::FileQueryInfoFlags::NONE,
            gtk::gio::Cancellable::NONE,
        )
        .ok()?;
    let mime = info.content_type()?.to_string();
    // Deliberately a list, not `image/*`: a type the engine cannot decode
    // would insert a broken <img> carrying the whole file as base64 — a RAW
    // photo or an HEIC off a phone is tens of megabytes of nothing. A
    // renderable format missing from this list merely becomes an attachment,
    // which is the harmless way to be wrong.
    const INLINE_MIMES: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "image/bmp",
        "image/svg+xml",
        "image/avif",
    ];
    (INLINE_MIMES.contains(&mime.as_str()) && info.size() as u64 <= MAX_INLINE_BYTES).then_some(mime)
}

/// Whether the file is a picture the editor would place in the text rather
/// than attach: what the composer's drop surfaces offer "Insert in Text" for.
pub fn is_inline_image(path: &std::path::Path) -> bool {
    inline_mime(&gtk::gio::File::for_path(path)).is_some()
}

/// What the format bar shows of where the caret is: the block toggles,
/// keyed the way `fmtState` in PASTE_SCRIPT reports them (`q` quote, `u`
/// bulleted list, `o` numbered list), and the paragraph-style, font and size
/// menus, whose labels name the caret's block, font and size (#358).
struct ToolbarState {
    toggles: Vec<(char, gtk::ToggleButton)>,
    style: gtk::MenuButton,
    font: gtk::MenuButton,
    size: gtk::MenuButton,
}

impl ToolbarState {
    /// Follow a `fmtState` report: `<toggles>|<block tag>|<font>|<points>`.
    fn update(&self, report: &str) {
        let mut parts = report.split('|');
        let toggles = parts.next().unwrap_or("");
        for (key, button) in &self.toggles {
            let on = toggles.contains(*key);
            if button.is_active() != on {
                button.set_active(on);
            }
        }
        let block = parts.next().unwrap_or("");
        let style = BLOCK_STYLES.iter().find(|(tag, _)| *tag == block).map_or(BLOCK_STYLES[0].1, |(_, l)| *l);
        self.style.set_label(&i18n(style));
        self.font.set_label(&font_label(parts.next().unwrap_or("")));
        let size = parts.next().unwrap_or("");
        if !size.is_empty() {
            self.size.set_label(size);
        }
    }
}

/// The font menu's label for the family `fmtState` reports: the name the
/// menu gives it, or a family set some other way (a custom pick, a quoted
/// message) by its own name.
fn font_label(family: &str) -> String {
    let key = family.to_lowercase();
    match FONTS.iter().find(|(k, _, _)| *k == key) {
        Some((_, _, label)) => i18n(label),
        None if family.is_empty() => i18n(FONTS[0].2),
        None if family.chars().count() > 18 => format!("{}…", family.chars().take(17).collect::<String>()),
        None => family.to_string(),
    }
}

/// The paragraph styles (#358): the `formatBlock` tag and its name.
const BLOCK_STYLES: &[(&str, &str)] = &[
    ("p", i18n_noop("Normal")),
    ("h1", i18n_noop("Heading 1")),
    ("h2", i18n_noop("Heading 2")),
    ("h3", i18n_noop("Heading 3")),
    ("pre", i18n_noop("Preformatted")),
];

/// The font families (#358): the family `fmtState` reports, lower case,
/// the CSS family list, and the name. The generic families come first, so
/// the recipient's own fonts stand in; the named ones are the fonts most
/// mail programs offer, each with fallbacks for a system without it. Their
/// names are not translated.
const FONTS: &[(&str, &str, &str)] = &[
    ("sans-serif", "sans-serif", i18n_noop("Sans Serif")),
    ("serif", "serif", i18n_noop("Serif")),
    ("monospace", "monospace", i18n_noop("Monospace")),
    ("arial", "Arial, Helvetica, sans-serif", "Arial"),
    ("verdana", "Verdana, Geneva, sans-serif", "Verdana"),
    ("tahoma", "Tahoma, Verdana, sans-serif", "Tahoma"),
    ("trebuchet ms", "'Trebuchet MS', Helvetica, sans-serif", "Trebuchet MS"),
    ("georgia", "Georgia, serif", "Georgia"),
    ("times new roman", "'Times New Roman', Times, serif", "Times New Roman"),
    ("garamond", "Garamond, serif", "Garamond"),
    ("courier new", "'Courier New', Courier, monospace", "Courier New"),
];

/// The font sizes (#358), in points, the unit mail programs show.
pub(crate) const SIZES: &[u32] = &[8, 9, 10, 11, 12, 14, 16, 18, 20, 24, 28, 36];

/// The color palette (#358): the GNOME palette, a column per hue from light
/// to dark, then the greys from white to black.
const PALETTE: &[[&str; 5]] = &[
    ["#99c1f1", "#62a0ea", "#3584e4", "#1c71d8", "#1a5fb4"],
    ["#8ff0a4", "#57e389", "#33d17a", "#2ec27e", "#26a269"],
    ["#f9f06b", "#f8e45c", "#f6d32d", "#f5c211", "#e5a50a"],
    ["#ffbe6f", "#ffa348", "#ff7800", "#e66100", "#c64600"],
    ["#f66151", "#ed333b", "#e01b24", "#c01c28", "#a51d2d"],
    ["#dc8add", "#c061cb", "#9141ac", "#813d9c", "#613583"],
    ["#cdab8f", "#b5835a", "#986a44", "#865e3c", "#63452c"],
    ["#ffffff", "#f6f5f4", "#deddda", "#c0bfbc", "#9a9996"],
    ["#77767b", "#5e5c64", "#3d3846", "#241f31", "#000000"],
];

/// A JavaScript string literal of `s`.
fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "''".into())
}

/// One command of the format bar: a flat icon button that never takes the
/// focus, so the editor keeps its selection.
fn bar_button(icon: &str, tip: &str) -> gtk::Button {
    let btn = gtk::Button::from_icon_name(icon);
    btn.set_tooltip_text(Some(i18n(tip).as_str()));
    btn.set_can_focus(false);
    btn.add_css_class("flat");
    btn
}

/// Run `js` in the editor, the caret first put back where it was when a
/// menu of the bar opened, and give the editor the focus again.
fn run_restored(webview: &gtk::glib::WeakRef<webkit6::WebView>, js: &str) {
    if let Some(wv) = webview.upgrade() {
        exec(&wv, &format!("window.__hylkiRestore&&window.__hylkiRestore();{js}"));
        wv.grab_focus();
    }
}

/// A menu of the bar. Opening it keeps the caret, which the popover's own
/// focus would otherwise cost the document. Its content scrolls, so that
/// [`fit_menu`] can keep it to the room there is.
fn bar_menu(webview: &webkit6::WebView, tip: &str, content: &impl IsA<gtk::Widget>) -> (gtk::MenuButton, gtk::Popover) {
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroller.set_propagate_natural_width(true);
    scroller.set_propagate_natural_height(true);
    scroller.set_child(Some(content));
    let popover = gtk::Popover::new();
    popover.set_child(Some(&scroller));
    popover.add_css_class("menu");
    let weak = webview.downgrade();
    popover.connect_show(move |_| {
        if let Some(wv) = weak.upgrade() {
            exec(&wv, "window.__hylkiSave&&window.__hylkiSave()");
        }
    });
    let btn = gtk::MenuButton::new();
    btn.set_popover(Some(&popover));
    btn.set_tooltip_text(Some(i18n(tip).as_str()));
    btn.set_can_focus(false);
    btn.add_css_class("flat");
    btn.set_create_popup_func(fit_menu);
    (btn, popover)
}

/// Keep a bar menu to the room above or below its button, whichever is
/// larger, scrolling past that. A popover taller than the room there is
/// never opened at all (#358): the font menu, with its recent fonts, did
/// not open from a composer near the foot of the screen.
fn fit_menu(btn: &gtk::MenuButton) {
    let Some(scroller) = btn.popover().and_then(|p| p.child()).and_downcast::<gtk::ScrolledWindow>() else {
        return;
    };
    let Some(root) = btn.root() else { return };
    let root = root.upcast_ref::<gtk::Widget>();
    let Some(at) = btn.compute_point(root, &gtk::graphene::Point::new(0.0, 0.0)) else {
        return;
    };
    let above = at.y() as i32;
    let below = root.height() - above - btn.height();
    // The popover's arrow, frame and padding, and a margin from the edge.
    scroller.set_max_content_height((above.max(below) - 40).max(120));
}

/// The font dialog's pick, or `None` when it is cancelled. Not the
/// binding's own `choose_family`: GTK answers a dialog accepted with no row
/// selected with no family and no error, which the binding takes for a
/// family and aborts on (#358).
pub(crate) fn choose_family(parent: Option<&gtk::Window>, done: impl FnOnce(Option<gtk::pango::FontFamily>) + 'static) {
    use gtk::glib::translate::{from_glib_full, ToGlibPtr};
    type Done = Box<dyn FnOnce(Option<gtk::pango::FontFamily>)>;
    unsafe extern "C" fn finish(
        source: *mut gtk::glib::gobject_ffi::GObject,
        result: *mut gtk::gio::ffi::GAsyncResult,
        data: gtk::glib::ffi::gpointer,
    ) {
        let done = unsafe { Box::from_raw(data as *mut Done) };
        let mut error = std::ptr::null_mut();
        let family = unsafe {
            gtk::ffi::gtk_font_dialog_choose_family_finish(source as *mut gtk::ffi::GtkFontDialog, result, &mut error)
        };
        if !error.is_null() {
            unsafe { gtk::glib::ffi::g_error_free(error) };
        }
        done((!family.is_null()).then(|| unsafe { from_glib_full(family) }));
    }
    let dialog = gtk::FontDialog::new();
    let data: Box<Done> = Box::new(Box::new(done));
    // The task GTK makes holds the dialog until `finish` has run.
    unsafe {
        gtk::ffi::gtk_font_dialog_choose_family(
            dialog.to_glib_none().0,
            parent.to_glib_none().0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            Some(finish),
            Box::into_raw(data) as gtk::glib::ffi::gpointer,
        );
    }
}

/// `font` as a CSS family list, with the generic family it falls back to
/// where the recipient does not have it.
pub(crate) fn recent_font_css(font: &crate::config::RecentFont) -> String {
    let name: String = font.name.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
    let generic = if font.mono { "monospace" } else { "sans-serif" };
    format!("'{name}', {generic}")
}

/// The editor command that sets `font`.
fn font_js(font: &crate::config::RecentFont) -> String {
    format!("window.__hylkiStyled('fontName',{})", js_str(&recent_font_css(font)))
}

/// The fonts Settings → Composing offers for new messages, as (CSS family
/// list, name) pairs: the default, the font menu's own, the recent picks,
/// and `current` when it is none of those.
pub(crate) fn compose_font_choices(current: &str) -> Vec<(String, String)> {
    let mut out = vec![(String::new(), i18n("Default"))];
    out.extend(FONTS.iter().map(|(_, css, name)| (css.to_string(), i18n(name))));
    let recent = crate::config::load_recent_fonts();
    for css in recent.iter().map(recent_font_css).chain((!current.is_empty()).then(|| current.to_string())) {
        if !out.iter().any(|(c, _)| c.eq_ignore_ascii_case(&css)) {
            let name = first_family(&css).to_string();
            out.push((css, name));
        }
    }
    out
}

/// A CSS family list's first family, unquoted.
fn first_family(css: &str) -> &str {
    css.split(',').next().unwrap_or("").trim().trim_matches(|c| c == '\'' || c == '"')
}

/// A family list from the settings file made safe to write into a style
/// sheet or a `style` attribute.
fn clean_family(css: &str) -> String {
    css.chars().filter(|c| !matches!(c, '"' | '<' | '>' | '{' | '}' | ';' | '\\' | '&') && !c.is_control()).collect::<String>().trim().to_string()
}

/// The editor's own font, which the signature and the quoted original
/// keep whatever font new messages are set to.
const EDITOR_FONT: &str = "14px/1.55 system-ui,sans-serif";

/// The body's `font` shorthand: the font and size set for new messages,
/// or the editor's own.
fn body_font() -> String {
    let prefs = crate::config::load_privacy();
    let family = clean_family(&prefs.compose_font);
    if family.is_empty() && prefs.compose_font_size == 0 {
        return EDITOR_FONT.to_string();
    }
    let family = if family.is_empty() { "system-ui,sans-serif".to_string() } else { family };
    let size = match prefs.compose_font_size {
        0 => "14px".to_string(),
        pt => format!("{pt}pt"),
    };
    format!("{size}/1.55 {family}")
}

/// The `style` the sent message's own writing is wrapped in, for the font
/// and size set for new messages: the editor's style sheet does not go out
/// with the message. Empty with neither set, leaving the recipient's font.
fn font_style(family: &str, pt: u32) -> String {
    let family = clean_family(family);
    let mut style = String::new();
    if !family.is_empty() {
        style.push_str(&format!("font-family:{family};"));
    }
    if pt > 0 {
        style.push_str(&format!("font-size:{pt}pt;"));
    }
    style
}

/// Draw a font menu item in the font it names (a CSS family list), so the
/// menu shows each font rather than naming it. A font not installed here
/// shows in the fallback the message itself would get.
pub(crate) fn preview_font(label: &gtk::Label, family: &str) {
    let family = family
        .split(',')
        .map(|f| f.trim().trim_matches(|c| c == '\'' || c == '"'))
        .collect::<Vec<_>>()
        .join(",");
    let attrs = gtk::pango::AttrList::new();
    attrs.insert(gtk::pango::AttrString::new_family(&family));
    label.set_attributes(Some(&attrs));
}

/// The recent fonts at the foot of the font menu: a divider and one item
/// per font, the one picked going back to the head of the list.
fn fill_recent_fonts(
    list: &gtk::Box,
    webview: &gtk::glib::WeakRef<webkit6::WebView>,
    popover: &gtk::glib::WeakRef<gtk::Popover>,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let fonts = crate::config::load_recent_fonts();
    list.set_visible(!fonts.is_empty());
    if fonts.is_empty() {
        return;
    }
    list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    for font in fonts {
        let text = gtk::Label::new(Some(&font.name));
        text.set_xalign(0.0);
        preview_font(&text, &font.name);
        let item = gtk::Button::new();
        item.set_child(Some(&text));
        item.add_css_class("flat");
        item.add_css_class("context-menu-item");
        let (weak, pop) = (webview.clone(), popover.clone());
        item.connect_clicked(move |_| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            run_restored(&weak, &font_js(&font));
            crate::config::note_recent_font(font.clone());
        });
        list.append(&item);
    }
}

/// A list of menu items, each running its command in the editor.
fn menu_items(webview: &webkit6::WebView, popover: &gtk::glib::WeakRef<gtk::Popover>, items: &[(String, String, Option<&str>)]) -> gtk::Box {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for (label, js, class) in items {
        let text = gtk::Label::new(Some(label));
        text.set_xalign(0.0);
        if let Some(c) = class {
            text.add_css_class(c);
        }
        let item = gtk::Button::new();
        item.set_child(Some(&text));
        item.add_css_class("flat");
        item.add_css_class("context-menu-item");
        let (weak, pop, js) = (webview.downgrade(), popover.clone(), js.clone());
        item.connect_clicked(move |_| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            run_restored(&weak, &js);
        });
        list.append(&item);
    }
    list
}

/// A color menu (#358): a clearing item, the palette, and Custom… for any
/// other color. `cmd` is the editing command (`foreColor`, `hiliteColor`),
/// and `glyph` the button's face, repainted with the color last chosen.
fn color_menu(
    webview: &webkit6::WebView,
    tip: &str,
    clear: &str,
    cmd: &'static str,
    none: &'static str,
    (glyph, repaint): (gtk::Widget, std::rc::Rc<dyn Fn()>),
    chosen: std::rc::Rc<std::cell::RefCell<Option<gtk::gdk::RGBA>>>,
) -> gtk::MenuButton {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let grid = gtk::Grid::new();
    grid.set_row_spacing(2);
    grid.set_column_spacing(2);
    let (btn, popover) = bar_menu(webview, tip, &content);
    btn.set_child(Some(&glyph));
    let weak_pop = popover.downgrade();
    // The one place every way of choosing ends: the button's face, then the
    // command, with the caret put back first.
    let apply: std::rc::Rc<dyn Fn(Option<gtk::gdk::RGBA>)> = {
        let (weak, repaint, chosen) = (webview.downgrade(), repaint.clone(), chosen.clone());
        std::rc::Rc::new(move |color| {
            let value = color.as_ref().map_or_else(|| none.to_string(), crate::color::to_hex);
            *chosen.borrow_mut() = color;
            repaint();
            run_restored(&weak, &format!("window.__hylkiStyled('{cmd}',{})", js_str(&value)));
        })
    };
    let reset = gtk::Button::with_label(&i18n(clear));
    reset.add_css_class("flat");
    {
        let (pop, apply) = (weak_pop.clone(), apply.clone());
        reset.connect_clicked(move |_| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            apply(None);
        });
    }
    content.append(&reset);
    for (col, hue) in PALETTE.iter().enumerate() {
        for (row, color) in hue.iter().enumerate() {
            let swatch = gtk::Button::new();
            swatch.set_child(Some(&crate::ui::context_menu::swatch_widget(color, true)));
            swatch.add_css_class("flat");
            swatch.set_tooltip_text(Some(color));
            let (pop, apply) = (weak_pop.clone(), apply.clone());
            swatch.connect_clicked(move |_| {
                if let Some(p) = pop.upgrade() {
                    p.popdown();
                }
                apply(gtk::gdk::RGBA::parse(*color).ok());
            });
            grid.attach(&swatch, col as i32, row as i32, 1, 1);
        }
    }
    content.append(&grid);
    let custom = gtk::Button::with_label(&i18n("Custom…"));
    custom.add_css_class("flat");
    {
        let (pop, weak, apply, chosen) = (weak_pop.clone(), webview.downgrade(), apply.clone(), chosen.clone());
        custom.connect_clicked(move |b| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            let picker = gtk::ColorDialog::new();
            picker.set_with_alpha(false);
            let initial = chosen.borrow().clone().unwrap_or(gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0));
            let parent = b.root().and_downcast::<gtk::Window>();
            let (weak, apply) = (weak.clone(), apply.clone());
            picker.choose_rgba(parent.as_ref(), Some(&initial), None::<&gtk::gio::Cancellable>, move |res| match res {
                Ok(rgba) => apply(Some(rgba)),
                // Cancelled: the caret goes back where it was all the same.
                Err(_) => run_restored(&weak, ""),
            });
        });
    }
    content.append(&custom);
    btn
}

/// The face of a color button: an "A" over a bar in the color last chosen
/// (text color), or an "A" on it (highlight). With none chosen, the bar
/// takes the text's own color and the highlight is left out. Returns the
/// face and what repaints it.
fn color_glyph(
    highlight: bool,
    chosen: std::rc::Rc<std::cell::RefCell<Option<gtk::gdk::RGBA>>>,
) -> (gtk::Widget, std::rc::Rc<dyn Fn()>) {
    let area = gtk::DrawingArea::new();
    area.set_content_width(18);
    area.set_content_height(18);
    let letter = gtk::Label::new(Some("A"));
    letter.add_css_class("heading");
    let face = gtk::Overlay::new();
    face.set_child(Some(&area));
    face.add_overlay(&letter);
    face.set_valign(gtk::Align::Center);
    if !highlight {
        letter.set_valign(gtk::Align::Start);
        letter.set_margin_top(-2);
    }
    {
        let chosen = chosen.clone();
        area.set_draw_func(move |area, cr, w, h| {
            let fg = area.color();
            let (w, h) = (w as f64, h as f64);
            let pick = *chosen.borrow();
            match (highlight, pick) {
                (true, Some(c)) => {
                    cr.set_source_rgba(c.red().into(), c.green().into(), c.blue().into(), 1.0);
                    cr.rectangle(0.0, 0.0, w, h);
                    let _ = cr.fill();
                }
                (true, None) => {
                    cr.set_source_rgba(fg.red().into(), fg.green().into(), fg.blue().into(), 0.5);
                    cr.set_line_width(1.0);
                    cr.rectangle(0.5, 0.5, w - 1.0, h - 1.0);
                    let _ = cr.stroke();
                }
                (false, c) => {
                    let c = c.unwrap_or(fg);
                    cr.set_source_rgba(c.red().into(), c.green().into(), c.blue().into(), 1.0);
                    cr.rectangle(2.0, h - 3.0, w - 4.0, 3.0);
                    let _ = cr.fill();
                }
            }
        });
    }
    // The letter is dark on a highlight, whatever the theme.
    let repaint: std::rc::Rc<dyn Fn()> = {
        let (area, letter) = (area.clone(), letter.clone());
        std::rc::Rc::new(move || {
            area.queue_draw();
            if highlight && chosen.borrow().is_some() {
                letter.add_css_class("on-highlight");
            } else {
                letter.remove_css_class("on-highlight");
            }
        })
    };
    (face.upcast(), repaint)
}

/// The format bar: the character styles, lists, quote, link and Clear
/// formatting, all of which Markdown can say, then a chevron that shows
/// paragraph styles and fonts, colors, indents, emoji and pictures (#358),
/// shown from the start when `expanded`. Its groups wrap onto a second row
/// in a narrow composer.
fn build_toolbar(
    webview: &webkit6::WebView,
    anchor: &gtk::Stack,
    attach_cb: &std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>>,
    expanded: bool,
) -> (ToolFlow, ToolFlow, gtk::Box, ToolbarState) {
    // The formatting commands flow and wrap across the bar, and a host can
    // hide them (a message written as source has no use for them) while the
    // bar itself stays: the format chooser holds its top right corner, and
    // only the first row stops short of it.
    let group = ToolFlow::new();
    group.add_css_class("toolbar");
    group.add_css_class("format-bar");
    group.set_hexpand(true);
    let end = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    // Each group of tools wraps too, so a composer narrower than a group
    // still takes its buttons one by one rather than growing past its pane.
    let cluster = || {
        let c = adw::WrapBox::new();
        c.set_child_spacing(2);
        c.set_line_spacing(2);
        c
    };

    // Paragraph style and font, each a menu whose label follows the caret.
    let styles = cluster();
    let style_items: Vec<(String, String, Option<&str>)> = BLOCK_STYLES
        .iter()
        .map(|(tag, label)| {
            let class = match *tag {
                "h1" => Some("title-2"),
                "h2" => Some("title-3"),
                "h3" => Some("heading"),
                "pre" => Some("monospace"),
                _ => None,
            };
            (i18n(label), format!("window.__hylkiBlock('{tag}')"), class)
        })
        .collect();
    let style_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    style_box.add_css_class("context-menu-list");
    let (style, style_pop) = bar_menu(webview, i18n_noop("Paragraph style"), &style_box);
    style_box.append(&menu_items(webview, &style_pop.downgrade(), &style_items));
    style.set_label(&i18n(BLOCK_STYLES[0].1));
    style.set_always_show_arrow(true);
    let font_items: Vec<(String, String, Option<&str>)> = FONTS
        .iter()
        .map(|(_, family, label)| (i18n(label), format!("window.__hylkiStyled('fontName',{})", js_str(family)), None))
        .collect();
    let font_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    font_box.add_css_class("context-menu-list");
    let (font, font_pop) = bar_menu(webview, i18n_noop("Font"), &font_box);
    let font_list = menu_items(webview, &font_pop.downgrade(), &font_items);
    let mut item = font_list.first_child();
    for (_, family, _) in FONTS {
        let Some(button) = item else { break };
        if let Some(label) = button.downcast_ref::<gtk::Button>().and_then(|b| b.child()).and_downcast::<gtk::Label>() {
            preview_font(&label, family);
        }
        item = button.next_sibling();
    }
    font_box.append(&font_list);
    // Any installed font. The recipient may not have it, so the family's
    // generic kind follows it.
    let other_font = gtk::Button::new();
    let other_label = gtk::Label::new(Some(&i18n("Other Font…")));
    other_label.set_xalign(0.0);
    other_font.set_child(Some(&other_label));
    other_font.add_css_class("flat");
    other_font.add_css_class("context-menu-item");
    {
        let (weak, pop) = (webview.downgrade(), font_pop.downgrade());
        other_font.connect_clicked(move |b| {
            if let Some(p) = pop.upgrade() {
                p.popdown();
            }
            let parent = b.root().and_downcast::<gtk::Window>();
            let weak = weak.clone();
            choose_family(parent.as_ref(), move |family| match family {
                Some(family) => {
                    let font = crate::config::RecentFont { name: family.name().to_string(), mono: family.is_monospace() };
                    run_restored(&weak, &font_js(&font));
                    crate::config::note_recent_font(font);
                }
                None => run_restored(&weak, ""),
            });
        });
    }
    font_box.append(&other_font);
    // The fonts picked there last (#358), at the foot of the menu, read
    // afresh each time it opens so that every composer offers the same.
    let recent_fonts = gtk::Box::new(gtk::Orientation::Vertical, 0);
    {
        let (weak, pop, recent) = (webview.downgrade(), font_pop.downgrade(), recent_fonts.clone());
        font.set_create_popup_func(move |btn| {
            fill_recent_fonts(&recent, &weak, &pop);
            fit_menu(btn);
        });
    }
    font_box.append(&recent_fonts);
    let prefs = crate::config::load_privacy();
    font.set_label(&font_label(first_family(&prefs.compose_font)));
    font.set_always_show_arrow(true);
    let size_items: Vec<(String, String, Option<&str>)> = std::iter::once((i18n("Default"), "window.__hylkiSize(0)".to_string(), None))
        .chain(SIZES.iter().map(|pt| (pt.to_string(), format!("window.__hylkiSize({pt})"), None)))
        .collect();
    let size_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    size_box.add_css_class("context-menu-list");
    let (size, size_pop) = bar_menu(webview, i18n_noop("Font size"), &size_box);
    size_box.append(&menu_items(webview, &size_pop.downgrade(), &size_items));
    // The size new messages start in (the editor's own 14px by default),
    // until the caret reports.
    size.set_label(&match prefs.compose_font_size {
        0 => "11".to_string(),
        pt => pt.to_string(),
    });
    size.set_always_show_arrow(true);
    styles.append(&style);
    styles.append(&font);
    styles.append(&size);

    // (icon, tooltip, execCommand snippet).
    let simple = |cluster: &adw::WrapBox, commands: &[(&str, &str, &str)]| {
        for (icon, tip, cmd) in commands {
            let btn = bar_button(icon, tip);
            // Weak: the format-state handler the view's content manager
            // owns reaches these buttons, so a strong view here would be a
            // cycle.
            let weak = webview.downgrade();
            let cmd = cmd.to_string();
            btn.connect_clicked(move |_| {
                if let Some(wv) = weak.upgrade() {
                    exec(&wv, &cmd);
                }
            });
            cluster.append(&btn);
        }
    };

    let chars = cluster();
    simple(&chars, &[
        ("format-text-bold-symbolic", i18n_noop("Bold"), "document.execCommand('bold')"),
        ("format-text-italic-symbolic", i18n_noop("Italic"), "document.execCommand('italic')"),
        ("format-text-underline-symbolic", i18n_noop("Underline"), "document.execCommand('underline')"),
        ("format-text-strikethrough-symbolic", i18n_noop("Strikethrough"), "document.execCommand('strikeThrough')"),
    ]);
    group.append(&chars);

    let colors = cluster();
    let text_chosen = std::rc::Rc::new(std::cell::RefCell::new(None));
    colors.append(&color_menu(
        webview,
        i18n_noop("Text color"),
        i18n_noop("Automatic"),
        "foreColor",
        "inherit",
        color_glyph(false, text_chosen.clone()),
        text_chosen,
    ));
    let hl_chosen = std::rc::Rc::new(std::cell::RefCell::new(None));
    colors.append(&color_menu(
        webview,
        i18n_noop("Highlight"),
        i18n_noop("No highlight"),
        "hiliteColor",
        "transparent",
        color_glyph(true, hl_chosen.clone()),
        hl_chosen,
    ));

    // The block commands are toggles: the document reports whether the
    // caret is inside one, and each command both opens and leaves its block
    // (the lists through execCommand's own toggling, the quote through
    // __hylkiQuote). A toggle shows the pressed look while the caret is in
    // its block; the click flips it at once and the document's next state
    // report settles it, so it never sticks wrong.
    let mut toggles = Vec::new();
    let toggle = |icon: &str, tip: &str, cmd: &str, key: char, toggles: &mut Vec<(char, gtk::ToggleButton)>| {
        let t = gtk::ToggleButton::new();
        t.set_icon_name(icon);
        t.set_tooltip_text(Some(i18n(tip).as_str()));
        t.set_can_focus(false);
        t.add_css_class("flat");
        let weak = webview.downgrade();
        let cmd = cmd.to_string();
        t.connect_clicked(move |_| {
            if let Some(wv) = weak.upgrade() {
                exec(&wv, &cmd);
            }
        });
        toggles.push((key, t.clone()));
        t
    };
    let lists = cluster();
    lists.append(&toggle("view-list-bullet-symbolic", i18n_noop("Bulleted list"), "document.execCommand('insertUnorderedList')", 'u', &mut toggles));
    lists.append(&toggle("view-list-ordered-symbolic", i18n_noop("Numbered list"), "document.execCommand('insertOrderedList')", 'o', &mut toggles));
    group.append(&lists);
    // Inside a list, indenting nests the items under the one above.
    let indents = cluster();
    simple(&indents, &[
        ("format-indent-less-symbolic", i18n_noop("Decrease indent"), "window.__hylkiIndent(false)"),
        ("format-indent-more-symbolic", i18n_noop("Increase indent"), "window.__hylkiIndent(true)"),
    ]);

    let inserts = cluster();
    // Adwaita has no blockquote glyph; the bubble with its quote marks
    // reads as one.
    inserts.append(&toggle("format-quote-symbolic", i18n_noop("Quote"), "window.__hylkiQuote()", 'q', &mut toggles));
    let link = bar_button("insert-link-symbolic", i18n_noop("Insert link"));
    {
        let weak = webview.downgrade();
        link.connect_clicked(move |b| {
            if let Some(wv) = weak.upgrade() {
                prompt_link(&wv, b);
            }
        });
    }
    inserts.append(&link);
    group.append(&inserts);
    let media = cluster();
    let emoji = bar_button("face-smile-symbolic", i18n_noop("Insert emoji"));
    {
        let (weak, anchor) = (webview.downgrade(), anchor.downgrade());
        emoji.connect_clicked(move |_| {
            if let (Some(wv), Some(anchor)) = (weak.upgrade(), anchor.upgrade()) {
                show_emoji_chooser(&wv, &anchor);
            }
        });
    }
    media.append(&emoji);
    let picture = bar_button("insert-image-symbolic", i18n_noop("Insert picture"));
    {
        let (weak, cb) = (webview.downgrade(), attach_cb.clone());
        picture.connect_clicked(move |b| {
            let Some(wv) = weak.upgrade() else { return };
            pick_pictures(&wv, b, &cb);
        });
    }
    media.append(&picture);

    let tail = cluster();
    simple(&tail, &[("edit-clear-symbolic", i18n_noop("Clear formatting"), "document.execCommand('removeFormat')")]);
    group.append(&tail);

    // The rest (#358) behind a chevron, after everything above so showing
    // them moves nothing: paragraph style and font, colors, indents, emoji
    // and pictures. They slide out from the chevron, which rides along to
    // their end, and slide back into it. Settings → Composing → Formatting
    // toolbar says how a new message starts; the chevron changes it for
    // this one.
    // A wrapping box, so in a narrow composer the groups take further rows
    // one by one rather than holding the toolbar, and the window, wider
    // than the pane.
    let extras = adw::WrapBox::new();
    extras.set_child_spacing(10);
    extras.set_line_spacing(2);
    for e in [&styles, &colors, &indents, &media] {
        extras.append(e);
    }
    let reveal = gtk::Revealer::new();
    reveal.set_transition_type(gtk::RevealerTransitionType::SlideRight);
    reveal.set_transition_duration(250);
    reveal.set_child(Some(&extras));
    // Where the tools do not fit beside the rest, they take the next row.
    // A growing revealer would start on this row and jump to the next
    // partway, so this spacer fills the rest of the row for the length of
    // the slide and the tools slide out on the next row from the start.
    let breaker = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    breaker.set_visible(false);
    // A folded revealer is still a child of the bar, and the bar's spacing
    // would show either side of it: it leaves the row once it has closed.
    {
        let breaker = breaker.clone();
        reveal.connect_child_revealed_notify(move |r| {
            breaker.set_visible(false);
            if !r.reveals_child() {
                r.set_visible(false);
            }
        });
    }
    let more = gtk::ToggleButton::new();
    more.set_can_focus(false);
    more.add_css_class("flat");
    let show_more = {
        let (reveal, breaker, group, tail, extras) =
            (reveal.clone(), breaker.clone(), group.clone(), tail.clone(), extras.clone());
        move |b: &gtk::ToggleButton| {
            let on = b.is_active();
            b.set_icon_name(if on { "pan-start-symbolic" } else { "pan-end-symbolic" });
            b.set_tooltip_text(Some(if on { i18n("Fewer formatting tools") } else { i18n("More formatting tools") }.as_str()));
            // The room left on the row after Clear formatting, against what
            // the tools and the chevron need.
            let gap = 10.0;
            let spare = group.room_after(tail.upcast_ref()).unwrap_or(0) as f32;
            let need = extras.measure(gtk::Orientation::Horizontal, -1).1 as f32 + gap + b.width() as f32;
            if reveal.transition_duration() > 0 && group.width() > 0 && need > spare {
                breaker.set_size_request((spare - 1.0).max(0.0) as i32, -1);
                breaker.set_visible(true);
            }
            if on {
                reveal.set_visible(true);
            }
            reveal.set_reveal_child(on);
        }
    };
    // How a new message starts: already open, with no slide.
    reveal.set_transition_duration(0);
    more.set_active(expanded);
    show_more(&more);
    reveal.set_visible(expanded);
    reveal.set_transition_duration(250);
    more.connect_toggled(show_more);
    // The chevron rides with the tools it shows, wrapping under them only
    // when the row truly has no room. A wrapping box of its own keeps the
    // toolbar's narrowest width that of the tools alone: a plain box would
    // add the chevron to it and push a narrow composer past its pane.
    let ride = adw::WrapBox::new();
    ride.set_child_spacing(10);
    ride.set_line_spacing(2);
    ride.append(&reveal);
    ride.append(&more);
    group.append(&breaker);
    group.append(&ride);
    // HYLKI_SHOWCASE_TOOLBAR_TOGGLE=<ms>: press the chevron that long after
    // the bar is built, so frames of the slide can be captured.
    if let Some(ms) = std::env::var("HYLKI_SHOWCASE_TOOLBAR_TOGGLE").ok().and_then(|v| v.parse::<u64>().ok()) {
        let more = more.downgrade();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            if let Some(m) = more.upgrade() {
                tracing::info!("showcase: chevron pressed");
                m.set_active(!m.is_active());
                // HYLKI_SHOWCASE_TOOLBAR_SHOTS=<png prefix>:<ms>,<ms>…
                // captures the window that long after the press.
                if let Some((prefix, times)) = std::env::var("HYLKI_SHOWCASE_TOOLBAR_SHOTS").ok().and_then(|v| {
                    v.rsplit_once(':').map(|(p, t)| (p.to_string(), t.to_string()))
                }) {
                    for t in times.split(',').filter_map(|t| t.parse::<u64>().ok()) {
                        let (root, path) = (m.root(), format!("{prefix}{t}.png"));
                        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(t), move || {
                            if let Some(r) = root {
                                crate::app::showcase_capture(r.upcast_ref::<gtk::Widget>(), &path);
                            }
                        });
                    }
                }
            }
        });
    }

    group.set_end(&end);
    (group.clone(), group, end, ToolbarState { toggles, style, font, size })
}

/// Choose pictures to put in the message at the caret (#358), the way a
/// drop or a paste puts them there: a file too big to carry in the body,
/// or not a picture, joins the attachments instead.
fn pick_pictures(
    webview: &webkit6::WebView,
    anchor: &gtk::Button,
    attach_cb: &std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>>,
) {
    exec(webview, "window.__hylkiSave&&window.__hylkiSave()");
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&i18n("Pictures")));
    filter.add_mime_type("image/*");
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let dialog = gtk::FileDialog::builder().title(i18n("Insert Picture")).filters(&filters).build();
    let parent = anchor.root().and_downcast::<gtk::Window>();
    let (weak, cb) = (webview.downgrade(), attach_cb.clone());
    dialog.open_multiple(parent.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
        let Some(wv) = weak.upgrade() else { return };
        let Ok(list) = res else { return };
        let files: Vec<gtk::gio::File> = (0..list.n_items())
            .filter_map(|i| list.item(i).and_downcast::<gtk::gio::File>())
            .collect();
        exec(&wv, "window.__hylkiRestore&&window.__hylkiRestore()");
        deliver_files(&wv, &files, None, &cb, true);
        wv.grab_focus();
    });
}

/// Prompt for a URL and turn the current selection into a link.
fn prompt_link(webview: &webkit6::WebView, anchor: &gtk::Button) {
    let parent = anchor.root().and_downcast::<gtk::Window>();
    let dialog = adw::MessageDialog::new(parent.as_ref(), Some(i18n("Insert Link").as_str()), None);
    dialog.add_response("cancel", &i18n("Cancel"));
    dialog.add_response("ok", &i18n("Insert"));
    dialog.set_default_response(Some("ok"));
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let entry = gtk::Entry::new();
    entry.set_input_purpose(gtk::InputPurpose::Url);
    entry.set_placeholder_text(Some("https://example.com"));
    entry.set_activates_default(true);
    dialog.set_extra_child(Some(&entry));
    let wv = webview.clone();
    dialog.connect_response(None, move |_, resp| {
        if resp == "ok" {
            let url = entry.text().to_string();
            let url = url.trim();
            if !url.is_empty() {
                exec(
                    &wv,
                    &format!("document.execCommand('createLink',false,'{}')", js_escape(url)),
                );
            }
        }
    });
    dialog.present();
}

/// The paste choke point: every paste — shortcut, context menu, editing
/// command — raises a DOM `paste` event here, where the clipboard's flavours
/// can be inspected. `__hylkiPasteRich` is the standing mode (the user's
/// preference, stamped at document build); `__hylkiPasteOnce` overrides it for
/// exactly one paste (Ctrl+V and the context-menu items set it just before
/// running the Paste command, so a live preference change is honored).
///
/// Plain mode inserts the clipboard's text flavour with URLs linkified; rich
/// mode lets WebKit's native paste keep the formatting, linkifying only
/// pastes that carry no HTML at all. Lives in `<head>` (not the editable
/// body) so it never becomes part of the message.
/// Cuts the body's text history into steps a person would recognise.
///
/// WebKit folds a whole run of typing into one undo step — and folds the
/// deletes that follow into that same step, so Backspace was nothing Ctrl+Z
/// could take back on its own, and a paragraph written over several minutes
/// came back all at once. Its history can be stepped but not shaped from
/// outside, and there is no API for "end the current step"; the one thing
/// that closes an open typing command is the selection changing. Re-setting
/// the selection to exactly where it already is does that and nothing else:
/// the caret does not move and no content is touched. (`Unselect` closes the
/// run too, but drops the caret and with it the next keystroke; blur/focus
/// works and is heavier than it needs to be.)
///
/// The run is closed when the kind of edit changes — typing to deleting or
/// back — and after a pause long enough that what follows is a new thought.
/// An IME composition is left alone: its own events flip between inserting
/// and deleting while a character is being built, and a break mid-word would
/// cut a single character into pieces.
///
/// Serves the source-mode document too, where the editable is a `<textarea>`
/// and the same close is `setSelectionRange` to the selection it already has.
const HISTORY_SCRIPT: &str = r#"<script>
(function(){
  /* How long a hand can rest before the next words are a step of their own. */
  var PAUSE_MS = 5000;
  var kind = null, timer = null, composing = false;
  document.addEventListener('compositionstart', function(){ composing = true; }, true);
  document.addEventListener('compositionend', function(){ composing = false; }, true);
  function closeRun(){
    var el = document.activeElement;
    if(el && (el.tagName === 'TEXTAREA' || el.tagName === 'INPUT')){
      try{
        el.setSelectionRange(el.selectionStart, el.selectionEnd, el.selectionDirection);
      }catch(_){}
      return;
    }
    var sel = getSelection();
    if(!sel || !sel.rangeCount) return;
    var range = sel.getRangeAt(0).cloneRange();
    sel.removeAllRanges();
    sel.addRange(range);
  }
  /* beforeinput, not input: the run has to be closed while the edit that
     starts the next one is still pending, or it lands in the old step. */
  document.addEventListener('beforeinput', function(e){
    if(composing) return;
    var now = /^delete/.test(e.inputType || '') ? 'delete' : 'insert';
    if(kind !== null && now !== kind) closeRun();
    kind = now;
    clearTimeout(timer);
    timer = setTimeout(function(){ closeRun(); kind = null; }, PAUSE_MS);
  }, true);
})();
</script>"#;

/// The editor's side of its emoji chooser (#348). `__hylkiEmojiMark` keeps
/// the caret, for the emoji to go to once the chooser has had the focus,
/// and says where it is (`x,y,w,h` in CSS pixels) for the chooser to point
/// at. `__hylkiEmojiPut` types the emoji there. A `<textarea>` (source
/// mode) keeps its own caret, and the chooser points at the field.
const EMOJI_SCRIPT: &str = r#"<script>
(function(){
  var at = null;
  function box(r){
    return [r.left, r.top, Math.max(1, r.width), Math.max(1, r.height)].map(Math.round).join(',');
  }
  window.__hylkiEmojiMark = function(){
    var el = document.activeElement;
    at = null;
    if(el && el.tagName === 'TEXTAREA') return box(el.getBoundingClientRect());
    var sel = getSelection();
    if(!sel || !sel.rangeCount) return '';
    at = sel.getRangeAt(0).cloneRange();
    var rects = at.getClientRects();
    if(rects.length) return box(rects[rects.length - 1]);
    /* A caret in an empty line has no box of its own. Its container is
       often the whole body, whose box would put the chooser under the
       message rather than under the caret: measure the line the caret is
       on, the child at its offset, by its start and one line high. */
    var n = at.startContainer;
    var e = n.nodeType === 3 ? n.parentNode : n;
    if(n.nodeType === 1 && n.childNodes.length){
      var c = n.childNodes[Math.min(at.startOffset, n.childNodes.length - 1)];
      e = c.nodeType === 3 ? n : c;
    }
    if(!e || !e.getBoundingClientRect) return '';
    var r = e.getBoundingClientRect();
    var lh = parseFloat(getComputedStyle(e).lineHeight)
      || parseFloat(getComputedStyle(e).fontSize) * 1.3 || 20;
    return box({left: r.left, top: r.top, width: 1, height: Math.min(r.height || lh, lh)});
  };
  window.__hylkiEmojiPut = function(text){
    if(at){
      var sel = getSelection();
      sel.removeAllRanges();
      sel.addRange(at);
      at = null;
    }
    document.execCommand('insertText', false, text);
  };
})();
</script>"#;

const PASTE_SCRIPT: &str = r#"<script>
(function(){
  window.__hylkiPasteOnce = null;
  var urlRe = /((?:https?:\/\/|www\.)[^\s<>()]+[^\s<>().,;:!?'"])/gi;
  function esc(s){return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');}
  /* Inline images (#113): a pasted or dropped image lands in the text as an
     <img src="data:..."> at the caret; the send path lifts it into a cid:
     part. Oversized images are downscaled on insert — a phone screenshot is
     thousands of pixels the recipient's pane will never show. */
  var MAXPX = 1600;
  function insertImageFile(f){
    if(!f) return;
    var r = new FileReader();
    r.onload = function(){
      scaleUrl(String(r.result), f.type, function(url){
        document.execCommand('insertHTML', false, '<img src="' + url + '" style="max-width:100%">');
      });
    };
    r.readAsDataURL(f);
  }
  function scaleUrl(url, type, done){
    var im = new Image();
    im.onload = function(){
      var w = im.naturalWidth, h = im.naturalHeight;
      if(w > MAXPX || h > MAXPX){
        var s = Math.min(MAXPX / w, MAXPX / h);
        var c = document.createElement('canvas');
        c.width = Math.round(w * s); c.height = Math.round(h * s);
        c.getContext('2d').drawImage(im, 0, 0, c.width, c.height);
        url = type === 'image/jpeg' ? c.toDataURL('image/jpeg', 0.85) : c.toDataURL('image/png');
      }
      done(url);
    };
    /* A picture the engine can't decode still has to let go of the queue
       below, or every image after it waits forever. */
    im.onerror = function(){ done(url); };
    im.src = url;
  }
  /* The way in for a file the widget read for us: dropped or pasted from a
     file manager, which the document itself never gets to see (the app's
     drop target and paste path explain why). One queue keeps a batch in the
     order it was dropped — each insert waits on an image decode, so parallel
     calls would race and land backwards. `x`/`y` place the caret at the drop
     point and are given for the first of a batch only; the rest follow the
     caret the previous insert left behind. */
  var insertQ = Promise.resolve();
  window.__hylkiInsertImageURL = function(url, type, name, x, y){
    insertQ = insertQ.then(function(){
      return new Promise(function(done){
        if(x !== null && document.caretRangeFromPoint){
          var r = document.caretRangeFromPoint(x, y);
          if(r){ var s = getSelection(); s.removeAllRanges(); s.addRange(r); }
        }
        /* No drop point and no caret in the text (the composer's drop
           surfaces, with the focus in the To row): the line written on
           first, at the top, rather than nowhere. */
        var cur = getSelection();
        if(!cur.rangeCount || !document.body.contains(cur.anchorNode)){
          var top = document.createRange();
          top.setStart(document.body.firstChild || document.body, 0);
          top.collapse(true);
          cur.removeAllRanges(); cur.addRange(top);
        }
        scaleUrl(url, type, function(u){
          /* The file's own name rides along as alt, which is the only place
             left to keep it: the picture is a data: URI by now, and the send
             path reads this back to name the cid: part. It is also what an
             alt attribute is for, so a recipient whose client blocks images
             reads the filename rather than nothing. */
          var a = name ? ' alt="' + esc(name).replace(/"/g, '&quot;') + '"' : '';
          document.execCommand('insertHTML', false,
            '<img src="' + u + '"' + a + ' style="max-width:100%">');
          done();
        });
      });
    });
  };
  /* WebKit's own paste inserts images as blob: URLs — invisible to
     clipboardData.items, dead on the wire, and never downscaled. Convert
     every blob: image that appears, from any entry path, into the same
     scaled data: URI a handled paste produces. */
  function adoptBlobImage(img){
    fetch(img.src).then(function(r){ return r.blob(); }).then(function(b){
      var rd = new FileReader();
      rd.onload = function(){
        scaleUrl(String(rd.result), b.type, function(url){
          img.src = url;
          img.style.maxWidth = '100%';
        });
      };
      rd.readAsDataURL(b);
    }).catch(function(){});
  }
  new MutationObserver(function(muts){
    muts.forEach(function(m){
      Array.prototype.forEach.call(m.addedNodes || [], function(n){
        if(n.nodeType !== 1) return;
        var imgs = n.tagName === 'IMG' ? [n]
          : (n.querySelectorAll ? n.querySelectorAll('img') : []);
        Array.prototype.forEach.call(imgs, function(im){
          if(im.src && im.src.indexOf('blob:') === 0) adoptBlobImage(im);
        });
      });
    });
  }).observe(document.documentElement, {childList: true, subtree: true});
  /* WebKit only marks a word's spelling once the caret leaves it. The word
     under the caret is checked by us instead: after a typing pause it goes
     to the app (which asks the same enchant the body's checker uses), and a
     misspelling is drawn through the CSS Custom Highlight API — no DOM
     mutation, so the caret, the undo stack, and the outgoing HTML are all
     untouched. The mark clears the moment typing resumes or the caret
     leaves the word, where WebKit's own marking takes over. */
  var spellT = null, composing = false, spellRange = null;
  var spellHl = (window.Highlight && window.CSS && CSS.highlights)
    ? new Highlight() : null;
  if(spellHl) CSS.highlights.set('hylki-misspell', spellHl);
  function caretWord(){
    var sel = getSelection();
    if(!sel.rangeCount || !sel.isCollapsed) return null;
    var n = sel.focusNode, off = sel.focusOffset;
    if(!n || n.nodeType !== 3) return null;
    var s = n.textContent;
    var isW = function(c){ return /[\p{L}'’]/u.test(c); };
    var a = off; while(a > 0 && isW(s[a-1])) a--;
    var b = off; while(b < s.length && isW(s[b])) b++;
    if(b - a < 2) return null;
    var r = document.createRange();
    r.setStart(n, a); r.setEnd(n, b);
    return {range: r, word: s.slice(a, b)};
  }
  window.__hylkiSpellMark = function(bad){
    if(!spellHl) return;
    spellHl.clear();
    if(bad && spellRange && spellRange.startContainer.isConnected){
      spellHl.add(spellRange);
    }
  };
  /* Which blocks the caret sits in, for the toolbar toggles: `q` quote,
     `u` bulleted list, `o` numbered list. Posted only when it changes. */
  var lastFmt = null;
  function caretBlock(){
    var sel = getSelection();
    var n = sel.rangeCount ? sel.anchorNode : null;
    return n && n.nodeType === 3 ? n.parentNode : n;
  }
  /* WebKit's Indent wraps the paragraph in a blockquote of its own, with
     no border: an indent, not a quote, for the Quote toggle and for Enter
     twice (#358). */
  function isIndent(b){
    return !!(b.style && b.style.borderStyle === 'none' && b.style.marginLeft);
  }
  function quoteOf(el){
    var b = el && el.closest ? el.closest('blockquote') : null;
    while(b && isIndent(b)){
      b = b.parentElement ? b.parentElement.closest('blockquote') : null;
    }
    return b;
  }
  /* The element the font and size menus describe: the caret's, or with a
     selection the first selected character's. The anchor would not do: a
     font applied to a selection leaves it at the end of the text before
     the new span, and the menu went on naming the old font (#358). */
  function styledEl(){
    var sel = getSelection();
    if(sel.rangeCount && !sel.isCollapsed){
      var r = sel.getRangeAt(0), root = r.commonAncestorContainer;
      if(root.nodeType === 3) return root.parentNode;
      var w = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      for(var t = w.nextNode(); t; t = w.nextNode()){
        if(!r.intersectsNode(t) || !t.nodeValue.trim()) continue;
        if(t === r.startContainer && r.startOffset >= t.length) continue;
        return t.parentNode;
      }
    }
    var el = caretBlock();
    return el && el.nodeType === 1 ? el : document.body;
  }
  /* The caret's font (#358): its first family, or the generic family the
     editor's own falls under. A font chosen for the next character typed
     comes first, then WebKit's own value, which the selection's anchor
     could not stand in for. */
  function fontOf(el){
    var g = pending && pending.fontFamily || '';
    if(!g) try{ g = String(document.queryCommandValue('fontName') || ''); }catch(_){}
    if(!g){ try{ g = getComputedStyle(el).fontFamily; }catch(_){} }
    var first = g.split(',')[0].replace(/["']/g, '').trim();
    if(first && !/^(system-ui|sans-serif|serif|monospace|ui-[a-z-]+|-webkit-[a-z-]+)$/i.test(first)) return first;
    g = g.toLowerCase();
    if(/mono/.test(g)) return 'monospace';
    if(/(^|,)\s*"?serif/.test(g) || (/serif/.test(g) && !/sans/.test(g))) return 'serif';
    return 'sans-serif';
  }
  /* The caret's text size in points, the unit the size menu shows: a size
     chosen for the next character typed, or the text's own. */
  function sizeOf(el){
    var want = pending && pending.fontSize || '';
    var px = 0;
    if(/pt$/.test(want)) return String(parseFloat(want));
    if(want) px = parseFloat(want);
    else try{ px = parseFloat(getComputedStyle(el).fontSize); }catch(_){}
    // A view not laid out yet reports nonsense; the label keeps what it had.
    return px >= 4 ? String(Math.round(px * 0.75)) : '';
  }
  function fmtState(){
    var el = caretBlock();
    var q = !!quoteOf(el);
    var ul = false, ol = false, block = '';
    try{
      ul = document.queryCommandState('insertUnorderedList');
      ol = document.queryCommandState('insertOrderedList');
      block = String(document.queryCommandValue('formatBlock') || '').toLowerCase();
    }catch(_){}
    var at = styledEl();
    var s = (q ? 'q' : '') + (ul ? 'u' : '') + (ol ? 'o' : '')
      + '|' + block + '|' + fontOf(at) + '|' + sizeOf(at);
    if(s === lastFmt) return;
    lastFmt = s;
    try{ window.webkit.messageHandlers.hylkiFormat.postMessage(s); }catch(_){}
  }
  document.addEventListener('selectionchange', fmtState);
  document.addEventListener('input', fmtState);
  /* The Quote button is a toggle (#137): inside a quote it steps the
     caret's paragraph out one level, the same move as Enter twice; anywhere
     else it quotes the paragraph. The list buttons toggle on their own. */
  window.__hylkiQuote = function(){
    var el = caretBlock();
    if(quoteOf(el)){
      document.execCommand('outdent');
    }else{
      document.execCommand('formatBlock', false, 'blockquote');
    }
    fmtState();
  };
  /* The format bar's menus take the focus while they are open, and the
     caret with it: kept when one opens, put back before its command. */
  var saved = null;
  window.__hylkiSave = function(){
    var sel = getSelection();
    saved = sel.rangeCount && document.body.contains(sel.anchorNode) ? sel.getRangeAt(0).cloneRange() : null;
  };
  window.__hylkiRestore = function(){
    if(!saved) return;
    var sel = getSelection();
    sel.removeAllRanges();
    sel.addRange(saved);
    saved = null;
  };
  /* Paragraph styles (#358): a heading, preformatted text or a plain
     paragraph. */
  window.__hylkiBlock = function(tag){
    document.execCommand('formatBlock', false, tag);
    fmtState();
  };
  /* Font and colors (#358) as inline styles, which every mail client
     keeps; WebKit's own markup is a <font> tag otherwise. Only for these
     commands: Bold and the rest stay the plain tags Markdown maps. */
  window.__hylkiStyled = function(cmd, value){
    var sel = getSelection();
    if(cmd === 'fontName' && sel.rangeCount && sel.isCollapsed){
      pend('fontFamily', value);
      return;
    }
    document.execCommand('styleWithCSS', false, true);
    document.execCommand(cmd, false, value);
    document.execCommand('styleWithCSS', false, false);
    fmtState();
  };
  /* A font or size chosen with nothing selected (#358) is for the next
     character typed, which opens a span of it, the way the other styles
     carry on from the caret. Not WebKit's own typing style, which any
     change of selection drops: picking a font in the font dialog takes the
     focus away from the document and back. */
  var pending = null;
  function pend(prop, value){
    pending = pending || {};
    pending[prop] = value;
    fmtState();
  }
  /* Font size (#358) in points. WebKit's own command knows only the seven
     <font size> steps, so the text is marked with the largest, which no
     message written here uses, and the mark turns into the size asked for
     (a <font> other mail kept as it was is left alone). 0 is the editor's
     own size. */
  function sizeValue(pt){
    return pt ? pt + 'pt' : getComputedStyle(document.body).fontSize;
  }
  function clearSizes(root){
    root.querySelectorAll('[style]').forEach(function(d){
      d.style.fontSize = '';
      if(!d.getAttribute('style')) d.removeAttribute('style');
    });
    root.querySelectorAll('font[size]').forEach(function(d){ d.removeAttribute('size'); });
  }
  window.__hylkiSize = function(pt){
    var sel = getSelection();
    if(!sel.rangeCount) return;
    var sizeWant = sizeValue(pt);
    if(sel.isCollapsed){
      pend('fontSize', sizeWant);
      return;
    }
    var before = new Set(document.body.querySelectorAll('font[size="7"]'));
    document.execCommand('styleWithCSS', false, false);
    document.execCommand('fontSize', false, '7');
    document.body.querySelectorAll('font[size="7"]').forEach(function(f){
      if(before.has(f)) return;
      clearSizes(f);
      f.removeAttribute('size');
      f.style.fontSize = sizeWant;
    });
    fmtState();
  };
  document.addEventListener('beforeinput', function(e){
    if(!pending || e.inputType !== 'insertText' || !e.data) return;
    var want = pending;
    pending = null;
    var sel = getSelection();
    if(!sel.rangeCount || !sel.isCollapsed) return;
    e.preventDefault();
    var span = document.createElement('span');
    if(want.fontFamily) span.style.fontFamily = want.fontFamily;
    if(want.fontSize) span.style.fontSize = want.fontSize;
    // A lone space at the end of a line would collapse, and the caret
    // with it out of the span.
    var text = document.createTextNode(e.data === ' ' ? '\u00a0' : e.data);
    span.appendChild(text);
    var r = sel.getRangeAt(0);
    r.insertNode(span);
    r.setStart(text, text.length);
    r.collapse(true);
    sel.removeAllRanges();
    sel.addRange(r);
    fmtState();
  });
  /* A font or size chosen with nothing selected is for the caret where
     it was. */
  function unpend(){
    if(!pending) return;
    pending = null;
    fmtState();
  }
  document.addEventListener('mousedown', unpend);
  document.addEventListener('keydown', function(e){
    if(e.key && e.key.indexOf('Arrow') === 0 || e.key === 'Home' || e.key === 'End') unpend();
  });
  /* Indent and outdent (#358). In a list they nest the item under the one
     above, or bring it back out. */
  window.__hylkiIndent = function(more){
    document.execCommand(more ? 'indent' : 'outdent');
    fmtState();
  };
  /* Return breaks the line, or with `__hylkiReturnParagraph` set starts a
     new paragraph, a <p> the recipient's client spaces as one; Shift+Return
     does the other. WebKit's own pair is the reverse of the default, and
     its paragraph is a copy of the block the caret is in, usually a
     spaceless <div>. Inside a list Return keeps making the next item.

     Enter twice inside a quote leaves it (#137), the way a list ends: the
     first Enter opens an empty line in the quote, the second takes that
     line out below it (WebKit's outdent drops one quote level for the
     caret's paragraph). Any other key in between makes the next Enter an
     ordinary one again. */
  var enterInQuote = false;
  if(window.__hylkiReturnParagraph){
    document.execCommand('defaultParagraphSeparator', false, 'p');
  }
  document.addEventListener('keydown', function(e){
    if(e.key !== 'Enter' || e.ctrlKey || e.altKey || e.metaKey || composing){
      enterInQuote = false;
      return;
    }
    var sel = getSelection();
    var n = sel.rangeCount ? sel.anchorNode : null;
    var el = n && n.nodeType === 3 ? n.parentNode : n;
    if(!el || !el.closest || !document.body.isContentEditable || el.closest('li')){
      enterInQuote = false;
      return;
    }
    var inQuote = !!quoteOf(el);
    if(!e.shiftKey && inQuote && enterInQuote && sel.isCollapsed){
      e.preventDefault();
      document.execCommand('outdent');
      enterInQuote = false;
      return;
    }
    enterInQuote = inQuote && !e.shiftKey;
    e.preventDefault();
    if(!window.__hylkiReturnParagraph === !e.shiftKey){
      document.execCommand('insertLineBreak');
      return;
    }
    document.execCommand('insertParagraph');
    /* A paragraph split from a <div> is another <div>; it becomes a <p>
       so it carries a paragraph's space. Only a bare div: one with a
       class (the signature) or a style is somebody's markup. */
    var m = getSelection().anchorNode;
    var b = m && (m.nodeType === 3 ? m.parentNode : m);
    b = b && b.closest && b.closest('p,div,li,blockquote,h1,h2,h3,h4,h5,h6,pre');
    if(b && b.tagName === 'DIV' && !b.attributes.length){
      document.execCommand('formatBlock', false, 'p');
    }
  }, true);
  document.addEventListener('compositionstart', function(){ composing = true; });
  document.addEventListener('compositionend', function(){ composing = false; });
  document.addEventListener('input', function(){
    if(!spellHl) return;
    spellHl.clear();
    if(composing) return;
    clearTimeout(spellT);
    spellT = setTimeout(function(){
      if(composing) return;
      var w = caretWord();
      if(!w) return;
      spellRange = w.range;
      try{ window.webkit.messageHandlers.hylkiSpell.postMessage(w.word); }catch(_){}
    }, 600);
  });
  document.addEventListener('selectionchange', function(){
    if(!spellHl || !spellRange) return;
    var sel = getSelection();
    if(!sel.rangeCount) return;
    var r = sel.getRangeAt(0);
    if(!sel.isCollapsed || r.startContainer !== spellRange.startContainer
       || r.startOffset < spellRange.startOffset
       || r.startOffset > spellRange.endOffset){
      spellHl.clear();
      spellRange = null;
    }
  });
  /* Resizing a picture. The frame and its corner handles are ordinary
     elements in this contenteditable document — there is nowhere else to
     put them — so they are marked `data-hylki-ui` and stripped from
     everything the document hands back (see `__hylkiBodyHtml`). They are
     also contenteditable=false, so the caret cannot wander into them. */
  var rsBox = null, rsImg = null;
  /* Blue is an ordinary selection; red says the picture will be recut when
     the message is sent, which is the one thing here that cannot be undone.
     The same red the spelling underline and the tray dot use. */
  var RS_BLUE = '#3584e4', RS_RED = '#e01b24';
  function rsRemove(){ if(rsBox){ rsBox.remove(); rsBox = null; rsImg = null; } }
  function rsTint(){
    if(!rsBox || !rsImg) return;
    var c = rsImg.hasAttribute('data-hylki-fit') ? RS_RED : RS_BLUE;
    rsBox.style.outlineColor = c;
    Array.prototype.forEach.call(rsBox.children, function(h){ h.style.background = c; });
  }
  function rsPlace(){
    if(!rsBox || !rsImg) return;
    if(!rsImg.isConnected){ rsRemove(); return; }
    var r = rsImg.getBoundingClientRect();
    rsBox.style.left = (r.left + scrollX) + 'px';
    rsBox.style.top = (r.top + scrollY) + 'px';
    rsBox.style.width = r.width + 'px';
    rsBox.style.height = r.height + 'px';
  }
  /* A width in CSS *and* in the attribute: mail clients that ignore styles
     still honour width, and the height is left to follow so the aspect can
     never be lost. */
  function rsApply(img, w){
    img.style.width = w + 'px';
    img.style.height = 'auto';
    img.setAttribute('width', String(w));
    img.removeAttribute('height');
  }
  function rsDown(e){
    if(!rsImg) return;
    e.preventDefault(); e.stopPropagation();
    var handle = e.currentTarget, img = rsImg;
    var west = handle.dataset.corner[1] === 'w';
    var startX = e.clientX, startW = img.getBoundingClientRect().width;
    var maxW = document.body.clientWidth;
    function move(ev){
      var dx = ev.clientX - startX;
      var w = Math.round(west ? startW - dx : startW + dx);
      rsApply(img, Math.max(24, Math.min(w, maxW)));
      rsPlace();
    }
    function up(ev){
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', up);
      try{ handle.releasePointerCapture(ev.pointerId); }catch(_){}
    }
    try{ handle.setPointerCapture(e.pointerId); }catch(_){}
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', up);
  }
  function rsShow(img){
    rsRemove();
    rsImg = img;
    rsBox = document.createElement('div');
    rsBox.setAttribute('data-hylki-ui', 'resize');
    rsBox.setAttribute('contenteditable', 'false');
    rsBox.style.cssText = 'position:absolute;pointer-events:none;z-index:9;'
      + 'outline:1px solid #3584e4;';
    ['nw','ne','sw','se'].forEach(function(c){
      var h = document.createElement('div');
      h.dataset.corner = c;
      h.style.cssText = 'position:absolute;width:11px;height:11px;'
        + 'background:#3584e4;border:2px solid Canvas;border-radius:50%;'
        + 'pointer-events:auto;cursor:'
        + ((c === 'nw' || c === 'se') ? 'nwse-resize' : 'nesw-resize') + ';';
      h.style[c[0] === 'n' ? 'top' : 'bottom'] = '-6px';
      h.style[c[1] === 'w' ? 'left' : 'right'] = '-6px';
      h.addEventListener('pointerdown', rsDown);
      rsBox.appendChild(h);
    });
    document.body.appendChild(rsBox);
    rsPlace();
    rsTint();
  }
  /* The context menu's size entries. `original` drops the explicit width so
     the picture falls back to its natural size, still held to the writing
     width by the stylesheet. */
  window.__hylkiSetImageSize = function(kind){
    var img = window.__hylkiCtxImg || rsImg;
    if(!img) return;
    if(kind === 'original'){
      img.style.width = ''; img.style.height = '';
      img.removeAttribute('width'); img.removeAttribute('height');
    } else {
      var full = document.body.clientWidth;
      var f = kind === 'small' ? 0.25 : (kind === 'medium' ? 0.5 : 1);
      rsApply(img, Math.max(24, Math.round(full * f)));
    }
    if(img === rsImg) rsPlace(); else rsShow(img);
  };
  /* Resizing a picture changes how big it is drawn, not how many bytes it
     is: the full-resolution data: URI still goes out, and the recipient can
     still save the original. Arming this asks for the bytes to be recut to
     the size shown, once, as the message is sent — irreversible, so it is
     off until asked for and marked on the picture while it is on. */
  window.__hylkiToggleFit = function(hint){
    var img = window.__hylkiCtxImg || rsImg;
    if(!img) return;
    if(img.hasAttribute('data-hylki-fit')){
      img.removeAttribute('data-hylki-fit');
      img.removeAttribute('title');
    } else {
      img.setAttribute('data-hylki-fit', '1');
      img.title = hint;
    }
    if(img === rsImg) rsTint(); else rsShow(img);
  };
  /* Recut every armed picture to the width it is drawn at. drawImage on an
     already-loaded image and toDataURL are both synchronous, so the send
     path can do this and read the body in one go. Enlarged pictures are left
     alone: there are no extra pixels to be had. */
  function rsFlatten(){
    Array.prototype.forEach.call(
      document.querySelectorAll('img[data-hylki-fit]'), function(im){
        var w = Math.round(im.getBoundingClientRect().width);
        if(!w || !im.naturalWidth || w >= im.naturalWidth) return;
        try{
          var c = document.createElement('canvas');
          c.width = w;
          c.height = Math.max(1, Math.round(w * im.naturalHeight / im.naturalWidth));
          c.getContext('2d').drawImage(im, 0, 0, c.width, c.height);
          var m = /^data:(image\/[a-z+]+)/.exec(im.src);
          var type = m ? m[1] : 'image/png';
          im.src = type === 'image/jpeg'
            ? c.toDataURL('image/jpeg', 0.85) : c.toDataURL('image/png');
        }catch(_){}
      });
  }
  /* What the host reads instead of body.innerHTML: the same content with
     this script's own furniture taken out. The arming mark stays, so a
     draft reopened later still shows it and still honours it when sent. */
  window.__hylkiBodyHtml = function(){
    var c = document.body.cloneNode(true);
    Array.prototype.forEach.call(c.querySelectorAll('[data-hylki-ui]'),
      function(n){ n.remove(); });
    return c.innerHTML;
  };
  /* The plain-text rendering of the body, for the text/plain alternative
     and for a message composed as plain text (#180): quoted blocks carry
     "> " on every line, lists their dashes, links their address, and
     block boundaries become line breaks. innerText knew none of that. */
  function hylkiTxt(node, pre){
    if(node.nodeType === 3){
      var v = node.nodeValue;
      return pre ? v : v.replace(/[ \t\r\n]+/g, ' ');
    }
    if(node.nodeType !== 1) return '';
    if(node.hasAttribute('data-hylki-ui')) return '';
    var tag = node.tagName.toLowerCase();
    if(tag === 'br') return '\n';
    if(tag === 'img') return node.getAttribute('alt') || '';
    if(tag === 'style' || tag === 'script') return '';
    var s = '';
    /* A quoted plain-text part keeps its spaces under an inline pre-wrap
       (#436), as <pre> does. */
    var inPre = pre || tag === 'pre' || /^(pre|break-spaces)/.test(node.style.whiteSpace || '');
    for(var i = 0; i < node.childNodes.length; i++) s += hylkiTxt(node.childNodes[i], inPre);
    if(tag === 'a'){
      var h = node.getAttribute('href') || '';
      if(h && h !== s.trim() && h.indexOf('mailto:') !== 0) s += ' <' + h + '>';
    }
    if(tag === 'li') return '- ' + s.replace(/^[ \n]+/, '') + '\n';
    if(tag === 'hr') return '\n----\n';
    if(tag === 'blockquote'){
      var lines = s.replace(/^\n+|\n+$/g, '').split('\n');
      return '\n' + lines.map(function(l){ return '> ' + l; }).join('\n') + '\n';
    }
    if(/^(p|div|h[1-6]|pre|tr|ul|ol|table|section|article|header|footer)$/.test(tag)) return '\n' + s + '\n';
    return s;
  }
  /* Trailing blanks go, except the one in a signature's "-- " line, which
     is how a mail client tells the signature from the message. */
  function tidyTxt(t){
    return t.replace(/[ \t]+\n/g, function(m, at, all){
        var start = all.lastIndexOf('\n', at - 1) + 1;
        return m === ' \n' && at - start === 2 && all.slice(start, at) === '--' ? m : '\n';
      })
      .replace(/\n{3,}/g, '\n\n')
      .replace(/^\n+|\n+$/g, '');
  }
  /* What is sent has spaces where the editor keeps no-break spaces to
     show them: a run of them typed, or the indents of a plain-text quote. */
  window.__hylkiBodyText = function(){
    return tidyTxt(hylkiTxt(document.body, false).replace(/\u00a0/g, ' '));
  };
  /* Plain text (#424) shows the message as it is sent: the document is
     flattened to the text above, the signature and the "wrote:" line
     keeping their blocks (and classes) so that changing the From, adding a
     signature or a cloud link finds them as in rich text. A quote becomes
     its "> " lines. */
  function plainTextOf(nodes){
    var d = document.createElement('div');
    nodes.forEach(function(n){ d.appendChild(n.cloneNode(true)); });
    return tidyTxt(hylkiTxt(d, false));
  }
  /* A line's indent, its runs of spaces and its tabs (to the next stop of
     eight) as no-break spaces: in a block that wraps they would collapse
     to one, on screen and in the text read back to send (#436). */
  function keepSpaces(line){
    var out = '', col = 0;
    for(var i = 0; i < line.length; i++){
      var c = line.charAt(i);
      if(c === '\t'){
        var n = 8 - col % 8;
        for(var k = 0; k < n; k++) out += '\u00a0';
        col += n;
        continue;
      }
      if(c === ' ' && (i === 0 || /[ \t\u00a0]/.test(line.charAt(i - 1)))) c = '\u00a0';
      out += c;
      col++;
    }
    return out;
  }
  function plainBlock(tag, cls, text){
    var e = document.createElement(tag);
    if(cls) e.className = cls;
    e.innerHTML = text ? text.split('\n').map(function(l){ return esc(keepSpaces(l)); }).join('<br>') : '<br>';
    return e;
  }
  function markOf(n){
    if(n.nodeType !== 1) return '';
    if(n.classList.contains('hylki-sig')) return 'hylki-sig';
    if(n.classList.contains('hylki-quote-attr')) return 'hylki-quote-attr';
    return '';
  }
  /* Whether `el`'s content is text and nothing else: line breaks, blocks
     with no styling of their own, and (with `quotes`) quotes of the same,
     which plain text writes as "> " lines. */
  function plainOk(el, quotes){
    var keep = quotes ? /^(DIV|P|BR|SPAN|BLOCKQUOTE)$/ : /^(DIV|P|BR|SPAN)$/;
    var all = [el].concat(Array.prototype.slice.call(el.querySelectorAll('*')));
    for(var i = 0; i < all.length; i++){
      var n = all[i];
      if(n.closest('[data-hylki-ui]')) continue;
      if(n === document.body) continue;
      if(!keep.test(n.tagName)) return false;
      for(var j = 0; j < n.attributes.length; j++){
        var a = n.attributes[j].name;
        if(a === 'style'){ if(n.getAttribute('style').trim()) return false; }
        else if(!/^(class|dir|id|data-.+)$/.test(a)) return false;
      }
    }
    return true;
  }
  /* Whether switching to plain text takes anything away: styling,
     pictures, links, lists. A quote is not lost, only written out. */
  window.__hylkiPlainLoses = function(){ return !plainOk(document.body, true); };
  function toPlain(){
    var b = document.body, segs = [], run = [];
    var flush = function(){ segs.push(['div', '', plainTextOf(run)]); run = []; };
    Array.prototype.slice.call(b.childNodes).forEach(function(n){
      if(n.nodeType === 1 && n.hasAttribute('data-hylki-ui')) return;
      var mark = markOf(n);
      if(!mark){ run.push(n); return; }
      flush();
      segs.push([n.tagName === 'P' ? 'p' : 'div', mark, plainTextOf([n])]);
    });
    flush();
    var out = document.createDocumentFragment();
    /* The first block stays even when empty: it is the line a new
       message or a reply is typed on. */
    segs.forEach(function(g, i){
      if(g[2] || i === 0) out.appendChild(plainBlock(g[0], g[1], g[2]));
    });
    b.textContent = '';
    b.appendChild(out);
  }
  /* What the composer runs after putting HTML of its own into a plain
     text body (a signature, a cloud link): any block that is not plain
     text yet is made so. */
  window.__hylkiPlainFix = function(){
    if(!window.__hylkiPlain) return;
    Array.prototype.slice.call(document.body.children).forEach(function(n){
      if(n.hasAttribute('data-hylki-ui') || plainOk(n, false)) return;
      n.replaceWith(plainBlock(n.tagName === 'P' ? 'p' : 'div', markOf(n), plainTextOf([n])));
    });
  };
  if(window.__hylkiPlain){
    document.addEventListener('DOMContentLoaded', toPlain);
    /* Bold, italic and underline are WebKit's own keys, not the
       toolbar's: in plain text they would show what is never sent. */
    document.addEventListener('keydown', function(e){
      if((e.ctrlKey || e.metaKey) && !e.altKey && /^[biu]$/i.test(e.key)) e.preventDefault();
    }, true);
    document.addEventListener('beforeinput', function(e){
      if(/^format/.test(e.inputType || '')) e.preventDefault();
    }, true);
  }
  /* Sending, and only sending: recut first, then read with the marks taken
     out. A draft is saved through the plain reader above, so quality is
     never lost to a save. */
  window.__hylkiBodyHtmlForSend = function(){
    rsFlatten();
    Array.prototype.forEach.call(
      document.querySelectorAll('img[data-hylki-fit]'), function(n){
        n.removeAttribute('data-hylki-fit'); n.removeAttribute('title');
      });
    rsTint();
    return fontRuns(window.__hylkiBodyHtml());
  };
  /* The font set for new messages, written around each stretch of what
     was typed, since the editor's style sheet does not go out with it. The
     signature and the quoted original (its attribution line and the quote
     straight after) are left out, as the editor shows them. */
  function fontRuns(html){
    var style = window.__hylkiComposeFont;
    if(!style) return html;
    var t = document.createElement('template');
    t.innerHTML = html;
    var kids = Array.prototype.slice.call(t.content.childNodes);
    var kept = kids.map(function(n){
      if(n.nodeType !== 1) return false;
      if(n.classList.contains('hylki-sig') || n.classList.contains('hylki-quote-attr')) return true;
      var prev = n.previousElementSibling;
      return n.tagName === 'BLOCKQUOTE' && !!prev && prev.classList.contains('hylki-quote-attr');
    });
    var run = [];
    function flush(){
      var typed = run.some(function(n){ return n.nodeType === 1 || /\S/.test(n.nodeValue || ''); });
      if(typed){
        var d = document.createElement('div');
        d.setAttribute('style', style);
        run[0].parentNode.insertBefore(d, run[0]);
        run.forEach(function(n){ d.appendChild(n); });
      }
      run = [];
    }
    kids.forEach(function(n, i){ if(kept[i]) flush(); else run.push(n); });
    flush();
    return t.innerHTML;
  }
  addEventListener('scroll', rsPlace, true);
  addEventListener('resize', rsPlace);
  document.addEventListener('input', rsPlace);
  /* Clicking an image selects it whole, so it can be deleted, cut, or
     copied like any other selection, and raises its resize frame. */
  document.addEventListener('click', function(e){
    var t = e.target;
    if(rsBox && rsBox.contains(t)) return;   // a handle, not the document
    if(t && t.tagName === 'IMG'){
      var r = document.createRange(); r.selectNode(t);
      var s = getSelection(); s.removeAllRanges(); s.addRange(r);
      rsShow(t);
    } else {
      rsRemove();
    }
  });
  /* A right-click on an image selects it (like a left click) and remembers
     the exact node: the menu's stock Cut/Copy then act on the image itself,
     and Send as Attachment Instead knows which one to lift. Captured on
     button-2 mousedown as well as contextmenu — whichever of the two WebKit
     actually delivers before it opens the menu. */
  function rememberImg(e){
    var t = e.target;
    if(t && t.tagName === 'IMG'){
      window.__hylkiCtxImg = t;
      var r = document.createRange(); r.selectNode(t);
      var s = getSelection(); s.removeAllRanges(); s.addRange(r);
      rsShow(t);
    } else if(e.type === 'contextmenu'){
      window.__hylkiCtxImg = null;
    }
  }
  document.addEventListener('mousedown', function(e){ if(e.button === 2) rememberImg(e); }, true);
  document.addEventListener('contextmenu', rememberImg, true);
  document.addEventListener('dragover', function(e){
    var it = (e.dataTransfer && e.dataTransfer.items) || [];
    for(var i = 0; i < it.length; i++){
      if(it[i].kind === 'file'){ e.preventDefault(); return; }
    }
  });
  /* Files from a file manager are taken by the widget's drop target before
     this runs — WebKitGTK leaves `dataTransfer.files` empty for them. What
     reaches here is a drag that really does carry file data, such as an
     image dragged out of another page. */
  document.addEventListener('drop', function(e){
    var fs = (e.dataTransfer && e.dataTransfer.files) || [];
    var imgs = [];
    for(var i = 0; i < fs.length; i++){ if(/^image\//.test(fs[i].type)) imgs.push(fs[i]); }
    if(!imgs.length) return;
    e.preventDefault();
    if(document.caretRangeFromPoint){
      var r = document.caretRangeFromPoint(e.clientX, e.clientY);
      if(r){ var sel = getSelection(); sel.removeAllRanges(); sel.addRange(r); }
    }
    imgs.forEach(insertImageFile);
  });
  function insertLinkified(text){
    urlRe.lastIndex = 0;
    var out='', last=0, m;
    while((m = urlRe.exec(text)) !== null){
      out += esc(text.slice(last, m.index));
      var url = m[0];
      var href = /^www\./i.test(url) ? 'http://'+url : url;
      out += '<a href="'+esc(href)+'">'+esc(url)+'</a>';
      last = m.index + url.length;
    }
    out += esc(text.slice(last));
    out = out.replace(/\r\n|\r|\n/g,'<br>');
    document.execCommand('insertHTML', false, out);
  }
  document.addEventListener('paste', function(e){
    var cd = e.clipboardData; if(!cd) return;
    /* Plain text takes the clipboard's text and nothing else: no
       formatting, no picture, no link made of an address. */
    if(window.__hylkiPlain){
      window.__hylkiPasteOnce = null;
      e.preventDefault();
      var t = cd.getData('text/plain');
      if(t) document.execCommand('insertHTML', false, esc(t).replace(/\r\n|\r|\n/g, '<br>'));
      return;
    }
    /* An image on the clipboard beats any text flavour riding along with it,
       and pastes inline whichever paste mode is set. */
    var its = cd.items || [];
    for(var i = 0; i < its.length; i++){
      if(its[i].kind === 'file' && /^image\//.test(its[i].type)){
        e.preventDefault();
        insertImageFile(its[i].getAsFile());
        return;
      }
    }
    var rich = window.__hylkiPasteRich === true;
    if(window.__hylkiPasteOnce !== null){ rich = window.__hylkiPasteOnce; window.__hylkiPasteOnce = null; }
    var html = cd.getData('text/html');
    var text = cd.getData('text/plain');
    if(rich){
      if(html) return;                       // native paste keeps the formatting
      if(!text) return;
      urlRe.lastIndex = 0;
      if(!urlRe.test(text)) return;          // plain text with nothing to linkify
    } else {
      if(!text) return;                      // no text flavour; let WebKit try
    }
    e.preventDefault();
    insertLinkified(text);
  });
})();
</script>"#;

/// The contentEditable HTML document, themed for light/dark. Its ground is
/// the theme's own view background (#148), read through `webview`, so a
/// custom GNOME theme reaches the editor as it does the reader — `Canvas`
/// would be WebKit's stock shade whatever the theme says.
///
/// With `plain` set the document is the plain-text view (#424): it
/// flattens what it is given to the text that is sent once it has parsed
/// it, and shows it in the desktop's monospace font. Every block of the
/// body is a paragraph there, a line's height above the next, because
/// that is the empty line the sent text has between them.
fn document(content: &str, webview: &webkit6::WebView, image_policy: &str, plain: bool) -> String {
    let dark = adw::StyleManager::default().is_dark();
    let scheme = if dark { "dark" } else { "light" };
    let (ground, _, _) = crate::ui::message_view::theme_grounds_for(webview, dark);
    let paste_rich = !crate::config::load_privacy().paste_plain;
    let return_paragraph = crate::config::load_privacy().return_paragraph;
    let body_font = body_font();
    let prefs = crate::config::load_privacy();
    let compose_font = js_str(&font_style(&prefs.compose_font, prefs.compose_font_size));
    let script = format!(
        "<script>window.__hylkiPasteRich={paste_rich};\
         window.__hylkiComposeFont={compose_font};\
         window.__hylkiReturnParagraph={return_paragraph};\
         window.__hylkiPlain={plain};</script>{PASTE_SCRIPT}{HISTORY_SCRIPT}{EMOJI_SCRIPT}"
    );
    let (plain_attr, plain_css) = if plain {
        let mono = monospace_family();
        (
            " data-plain",
            format!(
                "html[data-plain] body{{font:13px/1.6 {mono};}}\
                 html[data-plain] body *{{font:inherit!important;color:inherit;\
                   background:transparent;text-decoration:none;opacity:1;\
                   margin:0;padding:0;border:0;}}\
                 html[data-plain] body>*{{margin:0 0 1.6em;}}\
                 html[data-plain] body>*:last-child{{margin-bottom:0;}}"
            ),
        )
    } else {
        ("", String::new())
    };
    format!(
        "<!doctype html><html{plain_attr}><head><meta charset=\"utf-8\">\
         <meta name=\"color-scheme\" content=\"{scheme}\">{image_policy}\
         <style>\
           :root{{color-scheme:{scheme};}}\
           html,body{{height:100%;box-sizing:border-box;}}\
           body{{margin:0;padding:20px;font:{body_font};outline:none;\
             background:{ground};color:CanvasText;}}\
           /* Every image fits the writing width — pasted, dropped, or\
              quoted. The inline style on inserted images serves the\
              recipient; this rule is what the composer itself obeys,\
              whatever survives insertHTML. */\
           img{{max-width:100%;height:auto;}}\
           /* A picture armed to be recut when the message is sent. */\
           img[data-hylki-fit]{{outline:2px dashed #e01b24;outline-offset:2px;}}\
           /* The in-progress word's misspelling mark (Custom Highlight API);\
              the tint backs up engines that skip decorations in highlights. */\
           ::highlight(hylki-misspell){{\
             text-decoration:underline wavy #e01b24;\
             background-color:rgba(224,27,36,0.10);}}\
           blockquote{{margin:0;margin-inline-start:8px;padding-inline-start:10px;\
             border-inline-start:3px solid rgba(128,128,128,0.4);}}\
           /* Each paragraph written here takes the direction of its own\
              text (#366): Persian under English, or the other way round. */\
           body,body>div,body>p,li{{unicode-bidi:plaintext;}}\
           /* A paragraph's space goes below it, so the first line of the\
              message sits where it would in a <div>. */\
           body>p:first-child{{margin-top:0;}}\
           .hylki-quote-attr{{opacity:0.7;margin:10px 0 4px;}}\
           /* A quoted message that sets its own colors, on the light\
              ground it was designed for. */\
           blockquote.hylki-quote-mail{{background:#ffffff;color:#1e1e1e;\
             color-scheme:light;}}\
           .hylki-sig{{opacity:0.85;}}\
           /* The font set for new messages is for what is written here:\
              the signature and the quoted original keep the editor's. */\
           body>.hylki-sig,body>.hylki-quote-attr,\
           body>.hylki-quote-attr+blockquote{{font:{EDITOR_FONT};}}\
           a{{color:#3584e4;}}\
           {plain_css}\
         </style>{script}\
         <script>window.__hylkiDirty=false;\
           document.addEventListener('input',function(){{window.__hylkiDirty=true;}},true);\
         </script></head>\
         <body contenteditable=\"true\" dir=\"auto\">{content}</body></html>"
    )
}

/// The source-mode document: one textarea filling the view, in the
/// same ground as the rich editor so switching format does not change the
/// look of the pane. A textarea, rather than a plain-text `contenteditable`,
/// because what comes back out has to be exactly what was typed — no
/// normalised whitespace, no inserted line divs — and because undo, paste
/// and the engine's own spell checking all behave there without help.
///
/// The text goes in after a newline: an HTML parser drops the first newline
/// of a textarea's content, which took the blank line a Markdown reply opens
/// with and left the caret on the quote's "wrote:" line.
fn source_document(text: &str, webview: &webkit6::WebView) -> String {
    let dark = adw::StyleManager::default().is_dark();
    let scheme = if dark { "dark" } else { "light" };
    let (ground, _, _) = crate::ui::message_view::theme_grounds_for(webview, dark);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"color-scheme\" content=\"{scheme}\">\
         <style>\
           :root{{color-scheme:{scheme};}}\
           html,body{{height:100%;margin:0;background:{ground};}}\
           textarea{{display:block;width:100%;height:100%;box-sizing:border-box;\
             border:0;outline:none;resize:none;padding:18px 20px;\
             background:{ground};color:CanvasText;\
             font:13px/1.6 {mono};\
             tab-size:2;}}\
         </style></head><body>\
         <textarea id=\"src\" dir=\"auto\" spellcheck=\"true\" autocapitalize=\"off\" \
           autocorrect=\"off\">\n{text}</textarea>\
         <script>\
         (function(){{\
           var t=document.getElementById('src');\
           window.__hylkiDirty=false;\
           window.__hylkiSourceText=function(){{return t.value;}};\
           t.addEventListener('input',function(){{window.__hylkiDirty=true;}});\
           /* Tab indents instead of leaving the field: in a source view it\
              is a character, and there is nowhere else in the document to\
              tab to anyway. */\
           t.addEventListener('keydown',function(e){{\
             if(e.key!=='Tab'||e.ctrlKey||e.altKey)return;\
             e.preventDefault();\
             var a=t.selectionStart,b=t.selectionEnd;\
             t.setRangeText('  ',a,b,'end');\
             window.__hylkiDirty=true;\
           }});\
           t.focus();t.setSelectionRange(0,0);\
         }})();\
         </script>{HISTORY_SCRIPT}{EMOJI_SCRIPT}</body></html>",
        text = gtk::glib::markup_escape_text(text),
        mono = monospace_family(),
    )
}

/// The desktop's monospace font as a CSS family list, for the plain-text
/// and source views: the family picked for terminals and code, with the
/// generic family behind it. Read once, since asking costs a round trip to
/// the settings portal.
fn monospace_family() -> String {
    static FAMILY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FAMILY
        .get_or_init(|| css_monospace_family(&crate::desktop::monospace_font()))
        .clone()
}

/// A Pango font description's families, quoted, then `monospace`.
fn css_monospace_family(desc: &str) -> String {
    let fd = gtk::pango::FontDescription::from_string(desc);
    let family = fd.family().map(|f| f.to_string()).unwrap_or_default();
    let mut families: Vec<String> = family
        .split(',')
        .map(str::trim)
        .filter(|f| !f.is_empty() && !f.eq_ignore_ascii_case("monospace"))
        .map(|f| format!("\"{}\"", f.replace(['\\', '"', '<', '>', '{', '}', ';'], "")))
        .collect();
    families.push("monospace".to_string());
    families.join(",")
}

/// The preview document: the message as the recipient would see it, on the
/// editor's own ground.
fn preview_document(html: &str, webview: &webkit6::WebView, image_policy: &str) -> String {
    let dark = adw::StyleManager::default().is_dark();
    let scheme = if dark { "dark" } else { "light" };
    let (ground, _, _) = crate::ui::message_view::theme_grounds_for(webview, dark);
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"color-scheme\" content=\"{scheme}\">{image_policy}\
         <style>\
           :root{{color-scheme:{scheme};}}\
           html,body{{height:100%;box-sizing:border-box;}}\
           body{{margin:0;padding:20px;font:14px/1.55 system-ui,sans-serif;\
             background:{ground};color:CanvasText;}}\
           img{{max-width:100%;height:auto;}}\
           a{{color:#3584e4;}}\
         </style></head><body>{html}</body></html>"
    )
}

/// The `<meta>` that limits the pictures a document loads to embedded ones
/// and `allowed`, or nothing when `allowed` is `None`. Only `img-src` is
/// set: the editor's own scripts are inline and have to keep running.
fn image_policy_meta(allowed: Option<&[String]>) -> String {
    let Some(allowed) = allowed else { return String::new() };
    let mut sources = String::from("data: blob:");
    for url in allowed {
        // A source expression names a path, never a query, and must not
        // break the policy's own syntax.
        let url = url.split(['?', '#']).next().unwrap_or_default();
        let lower = url.to_ascii_lowercase();
        let usable = (lower.starts_with("https://") || lower.starts_with("http://"))
            && !url.contains(|c: char| c.is_whitespace() || matches!(c, ';' | ',' | '"' | '\'' | '<' | '>' | '&'));
        if usable {
            sources.push(' ');
            sources.push_str(url);
        }
    }
    format!("<meta http-equiv=\"Content-Security-Policy\" content=\"img-src {sources}\">")
}

/// Every remote picture `html` shows, by its `src` address.
pub fn remote_image_urls(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = html;
    while let Some(at) = find_src_attr(rest) {
        let tail = &rest[at..];
        match quoted_value(tail) {
            Some((prefix, value)) => {
                let lower = value.trim().to_ascii_lowercase();
                if lower.starts_with("https://") || lower.starts_with("http://") {
                    urls.push(value.trim().replace("&amp;", "&"));
                }
                rest = &tail[prefix.len() + value.len()..];
            }
            None => rest = &tail[4..],
        }
    }
    urls
}

/// Every dropped file straight onto the attachment list.
fn attach_all(
    files: &[gtk::gio::File],
    cb: &std::rc::Rc<std::cell::RefCell<Option<Box<dyn Fn(std::path::PathBuf)>>>>,
) -> bool {
    let mut took = false;
    for path in files.iter().filter_map(|f| f.path()) {
        match cb.borrow().as_ref() {
            Some(f) => {
                f(path);
                took = true;
            }
            None => tracing::warn!("editor drop: no attach callback set on this editor"),
        }
    }
    took
}

/// Escape a string for inclusion inside a single-quoted JavaScript string.
pub fn js_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' => out.push_str("\\x3c"),
            _ => out.push(c),
        }
    }
    out
}

/// Convert a stored signature (which may be legacy plain text) to HTML suitable
/// for the editor / for inserting into a message.
pub fn signature_to_html(sig: &str) -> String {
    if sig.contains('<') {
        sig.to_string()
    } else {
        gtk::glib::markup_escape_text(sig).replace('\n', "<br>")
    }
}

/// A signature brought in from outside (#120): an HTML file, or source
/// pasted into the signature's HTML dialog. Plain text (nothing that looks
/// like a tag) is escaped line by line like a typed signature.
///
/// HTML is the user's own material, but it still goes through the
/// sanitizer: a `<script>` or an event handler in a signature would run in
/// every composer, and a `<style>` block would restyle the whole message.
/// Structure and inline styles survive, since a designed signature is
/// nothing but tables and `style` attributes. An image referenced by a local
/// path is read and embedded as a `data:` URI (a relative path resolves
/// against `base`, the file's own directory), so the send path lifts it into
/// a `cid:` part like any inline picture; a remote image stays a remote URL.
pub fn signature_from_source(source: &str, base: Option<&std::path::Path>) -> String {
    if !source.contains('<') {
        return signature_to_html(source.trim_end());
    }
    let with_images = inline_local_images(source, base);
    let mut b = ammonia::Builder::default();
    b.add_tags(["table", "thead", "tbody", "tfoot", "tr", "td", "th", "font", "center", "u", "span", "img"])
        .add_generic_attributes([
            "style", "width", "height", "align", "valign", "bgcolor", "border", "cellpadding",
            "cellspacing", "color", "face", "size",
        ])
        .add_tag_attributes("img", ["src", "alt", "width", "height"])
        .url_schemes(std::collections::HashSet::from(["http", "https", "mailto", "tel", "data"]))
        .link_rel(None);
    b.clean(&with_images).to_string().trim().to_string()
}

/// Every `src="…"` that names a local image file, replaced by a `data:` URI
/// of the file's bytes. URLs (`http:`, `data:`, `cid:`, …) and anything that
/// is not a readable image of a sensible size are left exactly as written.
fn inline_local_images(html: &str, base: Option<&std::path::Path>) -> String {
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(at) = find_src_attr(rest) {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        // `tail` starts at `src=`; the value is the quoted run after it.
        let Some((prefix, value)) = quoted_value(tail) else {
            out.push_str(&tail[..4]);
            rest = &tail[4..];
            continue;
        };
        out.push_str(prefix);
        match local_image_data_uri(value, base, MAX_BYTES) {
            Some(uri) => out.push_str(&uri),
            None => out.push_str(value),
        }
        // The closing quote is the first char of `rest`, kept as it was.
        rest = &tail[prefix.len() + value.len()..];
    }
    out.push_str(rest);
    out
}

/// Byte offset of the next ` src=` attribute (any case), or `None`.
fn find_src_attr(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("src=") {
        let at = from + i;
        let preceded_by_space = at > 0 && lower.as_bytes()[at - 1].is_ascii_whitespace();
        if preceded_by_space {
            return Some(at);
        }
        from = at + 4;
    }
    None
}

/// Split `src="value"…` into (`src="`, `value`). `None` when the value is
/// unquoted or unterminated.
fn quoted_value(s: &str) -> Option<(&str, &str)> {
    let after = &s[4..];
    let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let value = &after[quote.len_utf8()..];
    let end = value.find(quote)?;
    Some((&s[..4 + quote.len_utf8()], &value[..end]))
}

/// The `data:` URI for a `src` value that names a local image, else `None`.
fn local_image_data_uri(value: &str, base: Option<&std::path::Path>, max: u64) -> Option<String> {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    let path = if let Some(rest) = lower.strip_prefix("file://") {
        std::path::PathBuf::from(crate::percent::decode(&v[v.len() - rest.len()..], false))
    } else if lower.contains(':') && !lower.starts_with('/') && !lower.starts_with('.') {
        // Some other scheme (http, https, data, cid, mailto…): not ours.
        return None;
    } else {
        let p = std::path::PathBuf::from(crate::percent::decode(v, false));
        if p.is_absolute() {
            p
        } else {
            base?.join(p)
        }
    };
    let mime = match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        _ => return None,
    };
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let data = std::fs::read(&path).ok()?;
    Some(format!("data:{mime};base64,{}", crate::oauth::base64_encode(&data)))
}

/// Re-ground an open editor document (#148): the color scheme and the page
/// ground are baked into the document when it loads, so a theme change — a
/// light/dark flip, or a new palette — has to be pushed into it. Deferred to
/// the next main-loop pass, because the theme's named colors are only
/// re-resolved after the signal that announced the change. `false` means the
/// view is gone and this editor needs telling no more.
fn reground(weak: &gtk::glib::WeakRef<webkit6::WebView>, dark: bool) -> bool {
    if weak.upgrade().is_none() {
        return false;
    }
    let weak = weak.clone();
    gtk::glib::idle_add_local_once(move || {
        let Some(v) = weak.upgrade() else { return };
        let (ground, _, _) = crate::ui::message_view::theme_grounds_for(&v, dark);
        let scheme = if dark { "dark" } else { "light" };
        exec(
            &v,
            &format!(
                "(function(){{\
                   var m=document.querySelector('meta[name=color-scheme]');\
                   if(m)m.content='{scheme}';\
                   document.documentElement.style.colorScheme='{scheme}';\
                   document.body.style.background='{ground}';\
                 }})()"
            ),
        );
    });
    true
}

#[cfg(test)]
mod signature_tests {
    use super::*;

    /// A reply to a message whose remote content is blocked still shows the
    /// user's own signature logo, and nothing else from the web (#295).
    #[test]
    fn blocked_images_spare_the_signature() {
        let sig = r#"<p>Jo</p><img src="https://cdn.example/logo.png?v=2&amp;x=1" alt="">
            <img src="data:image/png;base64,AAAA">"#;
        let urls = remote_image_urls(sig);
        assert_eq!(urls, vec!["https://cdn.example/logo.png?v=2&x=1".to_string()]);
        assert_eq!(
            image_policy_meta(Some(&urls)),
            "<meta http-equiv=\"Content-Security-Policy\" \
             content=\"img-src data: blob: https://cdn.example/logo.png\">"
        );
        assert_eq!(image_policy_meta(Some(&["https://x.example/a;b".to_string()])).matches("x.example").count(), 0);
        assert_eq!(image_policy_meta(None), "");
    }

    /// Plain text stays a typed signature: escaped, line breaks kept.
    #[test]
    fn plain_text_is_escaped_like_a_typed_signature() {
        assert_eq!(signature_from_source("Ada & Co\nCEO\n", None), "Ada &amp; Co<br>CEO");
    }

    /// The parts a designed signature is made of survive; what could run or
    /// restyle the message does not.
    #[test]
    fn html_keeps_structure_and_styles_but_not_scripts() {
        let src = "<html><head><style>body{color:red}</style></head><body>\
                   <table style=\"border:0\"><tr><td style=\"color:#333\" onclick=\"x()\">\
                   <b>Ada</b> <a href=\"https://example.com\">site</a></td></tr></table>\
                   <script>alert(1)</script></body></html>";
        let out = signature_from_source(src, None);
        assert!(out.contains("<table style=\"border:0\">"), "{out}");
        assert!(out.contains("<td style=\"color:#333\">"), "{out}");
        assert!(out.contains("<a href=\"https://example.com\">site</a>"), "{out}");
        assert!(!out.contains("onclick"), "{out}");
        assert!(!out.contains("script"), "{out}");
        assert!(!out.contains("color:red"), "{out}");
    }

    /// A local image next to the file is embedded; a remote one is left alone.
    #[test]
    fn local_images_are_embedded_and_remote_ones_kept() {
        let dir = std::env::temp_dir().join(format!("hylki-sig-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("logo.png"), [0x89, b'P', b'N', b'G']).unwrap();
        let src = "<p><img src=\"logo.png\" width=\"80\"> <img src='https://x.example/a.png'>\
                   <img src=\"missing.png\"></p>";
        let out = signature_from_source(src, Some(&dir));
        std::fs::remove_dir_all(&dir).ok();
        assert!(out.contains("src=\"data:image/png;base64,iVBORw==\""), "{out}");
        assert!(out.contains("width=\"80\""), "{out}");
        assert!(out.contains("src=\"https://x.example/a.png\""), "{out}");
        // A path that resolves to nothing is left as written.
        assert!(out.contains("src=\"missing.png\""), "{out}");
    }
}

impl Drop for RichEditor {
    /// The last handle takes the web process with it. A WebKit view keeps
    /// internal references to itself after it is unparented (measured: two
    /// on a bare view, one here), so it is never finalized by its owners
    /// letting go, and its WebKitWebProcess — a few hundred MB, invisible
    /// to the Flatpak sandbox's process tree — lived on for every composer
    /// ever closed (#221: six of them in one user's session monitor).
    /// Deferred past the composer's slide-out so the document does not
    /// blank mid-animation.
    fn drop(&mut self) {
        if std::rc::Rc::strong_count(&self._theme_handler) != 1 {
            return; // another handle still uses the view
        }
        let views: Vec<webkit6::WebView> =
            std::iter::once(self.webview.clone()).chain(self.preview.borrow().clone()).collect();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
            for v in &views {
                crate::memory_report::release_web_view(v);
            }
        });
    }
}

#[cfg(test)]
mod body_tests {
    use super::*;

    /// The text half survives the trip out of WebKit: a plain-text message
    /// went out empty when the halves were joined with a NUL (#297).
    #[test]
    fn both_halves_of_the_body_arrive() {
        let json = r#"["<p>Hi</p>","Hi\nthere \u0000 \"quoted\""]"#;
        assert_eq!(
            split_body(json),
            ("<p>Hi</p>".to_string(), "Hi\nthere \u{0} \"quoted\"".to_string())
        );
        assert_eq!(split_body("null"), (String::new(), String::new()));
    }

    /// The plain-text view's font is the desktop's monospace family, quoted
    /// so a name with spaces or digits holds, with the generic one behind
    /// it (#424).
    #[test]
    fn plain_text_uses_the_desktop_monospace_family() {
        assert_eq!(css_monospace_family("Adwaita Mono 11"), "\"Adwaita Mono\",monospace");
        assert_eq!(css_monospace_family("Source Code Pro Bold 10"), "\"Source Code Pro\",monospace");
        assert_eq!(css_monospace_family("Monospace 10"), "monospace");
        assert_eq!(css_monospace_family(""), "monospace");
        assert_eq!(css_monospace_family("Evil\"}body{x 10"), "\"Evilbodyx\",monospace");
    }
}

#[cfg(test)]
mod compose_font_tests {
    use super::*;

    /// The default font and size add no style, so the recipient's own font
    /// stays in charge.
    #[test]
    fn style_carries_the_chosen_font() {
        assert_eq!(font_style("", 0), "");
        assert_eq!(font_style("Georgia, serif", 12), "font-family:Georgia, serif;font-size:12pt;");
        assert_eq!(font_style("", 14), "font-size:14pt;");
    }

    /// A hand-edited settings file cannot close the attribute or the rule.
    #[test]
    fn family_cannot_escape_its_style() {
        assert_eq!(font_style("a\"><script>;}", 0), "font-family:ascript;");
        assert_eq!(first_family("'Comfortaa', sans-serif"), "Comfortaa");
    }
}
