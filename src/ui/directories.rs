//! Settings → LDAP Directories (#307): the company directories the composer
//! looks recipients up in. They are Evolution Data Server address books, so
//! this page lists the ones Evolution made as well, and the editor writes
//! the same source Evolution would.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use relm4::prelude::*;

use crate::directory::{self, Directory, Security};
use crate::i18n::{i18n, i18n_f};

pub struct Directories {
    dirs: Vec<Directory>,
    list: gtk::ListBox,
    empty: gtk::Label,
    add_btn: gtk::Button,
    toasts: adw::ToastOverlay,
    nav: adw::NavigationView,
    editor_page: adw::NavigationPage,
    editor_slot: adw::Bin,
    remove_btn: gtk::Button,
    /// Which directory the editor shows (none for a new one).
    editing: Option<usize>,
    /// What the editor's Save does: reads the form and sends `Save`;
    /// answers whether it accepted the form.
    save_action: Rc<RefCell<Option<Rc<dyn Fn() -> bool>>>>,
}

#[derive(Debug)]
pub enum DirectoriesInput {
    /// Ask EDS for the directories again.
    Load,
    Loaded(Result<Vec<Directory>, String>),
    Add,
    Edit(usize),
    ToggleEnabled { index: usize, enabled: bool },
    RemoveCurrent,
    ConfirmRemove(usize),
    Remove(usize),
    SaveClicked,
    CloseEditor,
    Save { dir: Directory, password: String },
    /// A save or removal went through (or not); the list follows, and a
    /// saved directory's connection check with it.
    Done { error: Option<String>, checked: Option<(String, Result<(), String>)> },
}

#[derive(Debug)]
pub enum DirectoriesOutput {
    /// The editor page is up (or gone): the settings window hides its
    /// shared header meanwhile and asks before moving on.
    EditorOpen(bool),
}

#[relm4::component(pub)]
impl SimpleComponent for Directories {
    type Init = ();
    type Input = DirectoriesInput;
    type Output = DirectoriesOutput;

    view! {
        adw::Bin {
            #[wrap(Some)]
            #[name = "nav"]
            set_child = &adw::NavigationView {
                add = &adw::NavigationPage {
                    set_title: &i18n("LDAP Directories"),
                    set_tag: Some("list"),

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        #[wrap(Some)]
                        #[name = "toasts"]
                        set_content = &adw::ToastOverlay {
                            #[wrap(Some)]
                            set_child = &adw::PreferencesPage {
                                add = &adw::PreferencesGroup {
                                    set_title: &i18n("LDAP directories"),
                                    set_description: Some(&i18n("The composer looks up recipients in these directories as you type an address, and a search in Contacts looks in them too. They are kept by Evolution Data Server, so a directory set up in Evolution is listed here too.")),
                                    #[wrap(Some)]
                                    #[name = "add_btn"]
                                    set_header_suffix = &gtk::Button {
                                        set_label: &i18n("Add Directory…"),
                                        set_valign: gtk::Align::Start,
                                        connect_clicked => DirectoriesInput::Add,
                                    },
                                    #[name = "list"]
                                    gtk::ListBox {
                                        add_css_class: "boxed-list",
                                        set_selection_mode: gtk::SelectionMode::None,
                                        set_margin_top: 16,
                                        set_visible: false,
                                        connect_row_activated[sender] => move |_, row| {
                                            sender.input(DirectoriesInput::Edit(row.index() as usize));
                                        },
                                    },
                                    #[name = "empty"]
                                    gtk::Label {
                                        set_label: &i18n("No directories yet."),
                                        add_css_class: "dim-label",
                                        set_wrap: true,
                                        set_margin_top: 12,
                                    },
                                },
                            },
                        },
                    },
                },

                #[name = "editor_page"]
                add = &adw::NavigationPage {
                    set_title: &i18n("LDAP Directory"),
                    set_tag: Some("editor"),

                    #[wrap(Some)]
                    set_child = &adw::ToolbarView {
                        add_top_bar = &adw::HeaderBar {
                            set_show_end_title_buttons: true,
                            pack_end = &gtk::Button {
                                set_label: &i18n("Save"),
                                add_css_class: "suggested-action",
                                connect_clicked => DirectoriesInput::SaveClicked,
                            },
                            #[name = "remove_btn"]
                            pack_end = &gtk::Button {
                                set_label: &i18n("Remove"),
                                add_css_class: "destructive-action",
                                set_visible: false,
                                connect_clicked => DirectoriesInput::RemoveCurrent,
                            },
                        },

                        #[wrap(Some)]
                        set_content = &gtk::ScrolledWindow {
                            set_hscrollbar_policy: gtk::PolicyType::Never,
                            #[wrap(Some)]
                            set_child = &adw::Clamp {
                                set_maximum_size: 640,
                                set_tightening_threshold: 480,
                                set_margin_top: 24,
                                set_margin_bottom: 24,
                                set_margin_start: 24,
                                set_margin_end: 24,
                                #[wrap(Some)]
                                #[name = "editor_slot"]
                                set_child = &adw::Bin {},
                            },
                        },
                    },
                },
            },
        }
    }

    fn init(_init: (), root: Self::Root, sender: ComponentSender<Self>) -> ComponentParts<Self> {
        let widgets = view_output!();
        let model = Directories {
            dirs: Vec::new(),
            list: widgets.list.clone(),
            empty: widgets.empty.clone(),
            add_btn: widgets.add_btn.clone(),
            toasts: widgets.toasts.clone(),
            nav: widgets.nav.clone(),
            editor_page: widgets.editor_page.clone(),
            editor_slot: widgets.editor_slot.clone(),
            remove_btn: widgets.remove_btn.clone(),
            editing: None,
            save_action: Rc::new(RefCell::new(None)),
        };
        {
            let s = sender.output_sender().clone();
            let save_action = model.save_action.clone();
            let slot = widgets.editor_slot.clone();
            widgets.nav.connect_visible_page_notify(move |nav| {
                let editor = nav.visible_page().and_then(|p| p.tag()).is_some_and(|t| t == "editor");
                if !editor {
                    *save_action.borrow_mut() = None;
                    slot.set_child(None::<&gtk::Widget>);
                }
                let _ = s.send(DirectoriesOutput::EditorOpen(editor));
            });
        }
        // Asked when the page is first shown, not with the settings window,
        // which is built ahead of time and may never open this page.
        {
            let s = sender.input_sender().clone();
            let loaded = std::cell::Cell::new(false);
            root.connect_map(move |_| {
                if !loaded.replace(true) {
                    let _ = s.send(DirectoriesInput::Load);
                }
            });
        }
        // HYLKI_SHOWCASE_EDIT_DIRECTORY=<index>|add opens that directory's
        // editor, or a new one, for a capture (demo only).
        if let Ok(what) = std::env::var("HYLKI_SHOWCASE_EDIT_DIRECTORY") {
            if std::env::var_os("HYLKI_DEMO").is_some() {
                let s = sender.input_sender().clone();
                gtk::glib::timeout_add_seconds_local_once(3, move || {
                    let _ = s.send(match what.as_str() {
                        "add" => DirectoriesInput::Add,
                        i => DirectoriesInput::Edit(i.parse().unwrap_or(0)),
                    });
                });
            }
        }
        ComponentParts { model, widgets }
    }

    fn update(&mut self, message: Self::Input, sender: ComponentSender<Self>) {
        match message {
            DirectoriesInput::Load => {
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let _ = s.send(DirectoriesInput::Loaded(directory::list()));
                });
            }
            DirectoriesInput::Loaded(result) => {
                match result {
                    Ok(dirs) => {
                        self.dirs = dirs;
                        self.add_btn.set_sensitive(true);
                        self.empty.set_label(&i18n("No directories yet."));
                    }
                    Err(_) => {
                        self.dirs.clear();
                        self.add_btn.set_sensitive(false);
                        self.empty.set_label(&i18n("Evolution Data Server, which keeps the directories, is not available on this system."));
                    }
                }
                self.rebuild(&sender);
            }
            DirectoriesInput::Add => self.open_editor(None, Directory::empty(), &sender),
            DirectoriesInput::Edit(i) => {
                if let Some(d) = self.dirs.get(i).cloned() {
                    self.open_editor(Some(i), d, &sender);
                }
            }
            DirectoriesInput::ToggleEnabled { index, enabled } => {
                let Some(d) = self.dirs.get_mut(index) else { return };
                if d.enabled == enabled {
                    return;
                }
                d.enabled = enabled;
                let d = d.clone();
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let error = directory::set_enabled(&d, enabled).err();
                    let _ = s.send(DirectoriesInput::Done { error, checked: None });
                });
            }
            DirectoriesInput::RemoveCurrent => {
                if let Some(i) = self.editing {
                    sender.input(DirectoriesInput::ConfirmRemove(i));
                }
            }
            DirectoriesInput::ConfirmRemove(i) => {
                let Some(d) = self.dirs.get(i) else { return };
                let parent = relm4::main_application().active_window();
                let dialog = adw::MessageDialog::new(
                    parent.as_ref(),
                    Some(&i18n_f("Remove {name}?", &[("name", &d.title())])),
                    Some(&i18n("The directory is removed from Evolution Data Server, so Evolution and other apps that use it stop seeing it too. Nothing on the server changes.")),
                );
                dialog.add_response("cancel", &i18n("Cancel"));
                dialog.add_response("remove", &i18n("Remove"));
                dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                let s = sender.input_sender().clone();
                dialog.connect_response(None, move |_, response| {
                    if response == "remove" {
                        let _ = s.send(DirectoriesInput::Remove(i));
                    }
                });
                dialog.present();
            }
            DirectoriesInput::Remove(i) => {
                let Some(d) = self.dirs.get(i).cloned() else { return };
                if self.editing == Some(i) {
                    self.editing = None;
                    self.close_editor();
                }
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let error = directory::remove(&d).err();
                    let _ = s.send(DirectoriesInput::Done { error, checked: None });
                });
            }
            DirectoriesInput::SaveClicked => {
                let action = self.save_action.borrow().clone();
                if let Some(f) = action {
                    if f() {
                        self.close_editor();
                    }
                }
            }
            DirectoriesInput::CloseEditor => self.close_editor(),
            DirectoriesInput::Save { dir, password } => {
                let s = sender.input_sender().clone();
                std::thread::spawn(move || {
                    let password = (!password.is_empty()).then_some(password);
                    let msg = match directory::save(&dir, password.as_deref()) {
                        Err(e) => DirectoriesInput::Done { error: Some(e), checked: None },
                        Ok(uid) if dir.enabled => {
                            // A new source takes a moment to appear in the
                            // registry; check it once it has.
                            let mut found = None;
                            for _ in 0..10 {
                                found = directory::list().ok().and_then(|l| l.into_iter().find(|d| d.uid == uid));
                                if found.is_some() {
                                    break;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(200));
                            }
                            // The list shows the new directory now, not once
                            // the server has answered, which can take a while.
                            let _ = s.send(DirectoriesInput::Load);
                            let checked = found.map(|d| (d.title(), check_within(d, CHECK_LIMIT)));
                            DirectoriesInput::Done { error: None, checked }
                        }
                        Ok(_) => DirectoriesInput::Done { error: None, checked: None },
                    };
                    let _ = s.send(msg);
                });
            }
            DirectoriesInput::Done { error, checked } => {
                if let Some(e) = error {
                    self.toast(&e);
                }
                match checked {
                    Some((name, Ok(()))) => self.toast(&i18n_f("Connected to {name}", &[("name", &name)])),
                    Some((name, Err(e))) => {
                        self.toast(&i18n_f("{name} did not answer: {error}", &[("name", &name), ("error", &e)]))
                    }
                    None => {}
                }
                sender.input(DirectoriesInput::Load);
            }
        }
    }
}

/// How long Settings waits on a directory's first answer. EDS sets no
/// timeout of its own on the connection, and holds the book while it waits.
const CHECK_LIMIT: std::time::Duration = std::time::Duration::from_secs(30);

/// [`directory::check`], given up on after `limit`; the check itself runs on
/// to its end in the background.
fn check_within(dir: Directory, limit: std::time::Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let name = dir.title();
    std::thread::spawn(move || {
        let _ = tx.send(directory::check(&dir));
    });
    rx.recv_timeout(limit).unwrap_or_else(|_| {
        tracing::warn!(directory = %name, "LDAP directory check gave no answer in {}s", limit.as_secs());
        Err(i18n("The server took too long to answer."))
    })
}

impl Directories {
    fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        toast.set_timeout(6);
        self.toasts.add_toast(toast);
    }

    fn open_editor(&mut self, index: Option<usize>, dir: Directory, sender: &ComponentSender<Self>) {
        self.editor_page.set_title(&if index.is_some() { i18n("Edit Directory") } else { i18n("Add Directory") });
        self.editing = index;
        self.remove_btn.set_visible(index.is_some());
        let (form, save) = build_editor(dir, sender.input_sender().clone());
        self.editor_slot.set_child(Some(&form));
        *self.save_action.borrow_mut() = Some(save);
        if !self.nav.visible_page().and_then(|p| p.tag()).is_some_and(|t| t == "editor") {
            self.nav.push_by_tag("editor");
        }
    }

    fn close_editor(&self) {
        if self.nav.visible_page().and_then(|p| p.tag()).is_some_and(|t| t == "editor") {
            self.nav.pop();
        }
    }

    fn rebuild(&mut self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for (i, d) in self.dirs.iter().enumerate() {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(true);
            let hbox = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            hbox.add_css_class("account-list-row");
            let icon = gtk::Image::from_icon_name("x-office-address-book-symbolic");
            icon.set_pixel_size(24);
            hbox.append(&icon);
            let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
            vbox.set_hexpand(true);
            vbox.set_valign(gtk::Align::Center);
            let title = gtk::Label::new(Some(&d.title()));
            title.set_halign(gtk::Align::Start);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.add_css_class("account-name");
            let mut sub = d.host.clone();
            if !d.base_dn.is_empty() {
                sub.push_str(&format!(" · {}", d.base_dn));
            }
            // Why the last lookup failed, until one succeeds: otherwise the
            // composer just never suggests anyone from it (#307).
            let failed = if d.enabled { directory::last_error(&d.uid) } else { None };
            let subtitle = gtk::Label::new(Some(failed.as_deref().unwrap_or(&sub)));
            subtitle.set_halign(gtk::Align::Start);
            subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
            subtitle.add_css_class("account-email");
            if let Some(e) = &failed {
                subtitle.add_css_class("error");
                subtitle.set_tooltip_text(Some(e));
            }
            vbox.append(&title);
            vbox.append(&subtitle);
            hbox.append(&vbox);
            let toggle = gtk::Switch::new();
            toggle.set_valign(gtk::Align::Center);
            toggle.set_tooltip_text(Some(&i18n("Look up recipients in this directory")));
            toggle.set_active(d.enabled);
            let s = sender.input_sender().clone();
            toggle.connect_state_set(move |_, state| {
                let _ = s.send(DirectoriesInput::ToggleEnabled { index: i, enabled: state });
                gtk::glib::Propagation::Proceed
            });
            hbox.append(&toggle);
            let next = gtk::Image::from_icon_name("go-next-symbolic");
            next.add_css_class("dim-label");
            hbox.append(&next);
            row.set_child(Some(&hbox));
            self.list.append(&row);
        }
        self.list.set_visible(!self.dirs.is_empty());
        self.empty.set_visible(self.dirs.is_empty());
    }
}

const SECURITY: [Security; 3] = [Security::StartTls, Security::Ldaps, Security::None];

/// The directory editor's form, and what the page's Save does with it.
fn build_editor(dir: Directory, sender: relm4::Sender<DirectoriesInput>) -> (gtk::Widget, Rc<dyn Fn() -> bool>) {
    let server = adw::PreferencesGroup::new();
    server.set_title(&i18n("Server"));
    let name = adw::EntryRow::new();
    name.set_title(&i18n("Name (optional)"));
    name.set_text(&dir.name);
    let host = adw::EntryRow::new();
    host.set_title(&i18n("Server"));
    host.set_text(&dir.host);
    host.set_input_purpose(gtk::InputPurpose::Url);
    let security = adw::ComboRow::new();
    security.set_title(&i18n("Encryption"));
    security.set_model(Some(&gtk::StringList::new(&[
        &i18n("StartTLS"),
        &i18n("TLS (LDAPS)"),
        &i18n("None"),
    ])));
    security.set_selected(SECURITY.iter().position(|s| *s == dir.security).unwrap_or(0) as u32);
    let port = adw::SpinRow::with_range(1.0, 65535.0, 1.0);
    port.set_title(&i18n("Port"));
    port.set_value(f64::from(dir.port));
    // Follow the encryption with the port, unless it was set to something
    // other than the usual one.
    {
        let port = port.clone();
        security.connect_selected_notify(move |row| {
            let chosen = SECURITY.get(row.selected() as usize).copied().unwrap_or(Security::StartTls);
            if SECURITY.iter().any(|s| f64::from(s.default_port()) == port.value()) {
                port.set_value(f64::from(chosen.default_port()));
            }
        });
    }
    let base = adw::EntryRow::new();
    base.set_title(&i18n("Search base, such as dc=example,dc=com"));
    base.set_text(&dir.base_dn);
    let scope = adw::ComboRow::new();
    scope.set_title(&i18n("Search"));
    scope.set_model(Some(&gtk::StringList::new(&[&i18n("Whole tree"), &i18n("First level")])));
    scope.set_selected(if dir.subtree { 0 } else { 1 });
    for w in [name.upcast_ref::<gtk::Widget>(), host.upcast_ref(), security.upcast_ref(), port.upcast_ref(), base.upcast_ref(), scope.upcast_ref()] {
        server.add(w);
    }

    let signin = adw::PreferencesGroup::new();
    signin.set_title(&i18n("Sign-in"));
    signin.set_description(Some(&i18n("Leave these empty for a directory that can be searched without signing in.")));
    let bind = adw::EntryRow::new();
    bind.set_title(&i18n("Sign in as, such as cn=jane,ou=people,dc=example,dc=com"));
    bind.set_text(&dir.bind_dn);
    let pass = adw::PasswordEntryRow::new();
    let saved = !dir.uid.is_empty() && directory::has_password(&dir);
    pass.set_title(&if saved { i18n("Password (saved; type to replace it)") } else { i18n("Password") });
    signin.add(&bind);
    signin.add(&pass);

    let error = gtk::Label::new(None);
    error.add_css_class("error");
    error.set_wrap(true);
    error.set_xalign(0.0);
    error.set_visible(false);

    let form = gtk::Box::new(gtk::Orientation::Vertical, 24);
    form.append(&server);
    form.append(&signin);
    form.append(&error);

    let save: Rc<dyn Fn() -> bool> = Rc::new(move || {
        // A server pasted as a URL names its encryption and port too.
        let (host_text, typed_security, typed_port) = directory::parse_server(&host.text());
        if host_text.is_empty() {
            error.set_label(&i18n("Enter the directory's server."));
            error.set_visible(true);
            host.grab_focus();
            return false;
        }
        let mut d = dir.clone();
        d.name = name.text().trim().to_string();
        d.host = host_text;
        let chosen = SECURITY.get(security.selected() as usize).copied().unwrap_or(Security::StartTls);
        d.security = typed_security.unwrap_or(chosen);
        d.port = match typed_port {
            Some(p) => p,
            // The usual port of the encryption picked, when the URL changed it.
            None if SECURITY.iter().any(|s| f64::from(s.default_port()) == port.value()) => d.security.default_port(),
            None => port.value() as u16,
        };
        d.base_dn = base.text().trim().to_string();
        d.subtree = scope.selected() == 0;
        d.bind_dn = bind.text().trim().to_string();
        let _ = sender.send(DirectoriesInput::Save { dir: d, password: pass.text().to_string() });
        true
    });
    (form.upcast(), save)
}
