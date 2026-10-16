//! A shared builder for right-click context menus, styled to match GNOME HIG:
//! a flat list of icon + label rows in a borderless `GtkPopover`, grouped into
//! sections by hairline separators. Built from plain widgets (not a
//! `Gio.Menu`/`GtkPopoverMenu`, which on GTK4 wraps everything in an internal
//! `GtkScrolledWindow`), so the popover and its box always size to exactly
//! what the sections need and never grow a scrollbar.
//!
//! Icons deliberately mirror the reader-toolbar buttons for the same actions,
//! tying the menu entries to the buttons users already know. In a menu where
//! any entry carries an icon, iconless entries get a blank slot of the same
//! width so every label stays aligned.
//!
//! An entry can open a submenu ([`MenuEntry::submenu`]): the popover slides
//! to a page of its own, headed by a back row, the way `GtkPopoverMenu`
//! nests — so a long list (every tag) never makes the main menu too tall.

use gtk::prelude::*;

/// One entry in a context menu: a label, an optional leading symbolic icon,
/// and the callback to run on activation. Use `.enabled(false)` to grey an
/// item out rather than hiding it — HIG prefers a disabled item users can
/// still find over one that vanishes and makes the menu shift under them.
pub struct MenuEntry {
    label: String,
    icon: Option<String>,
    /// A color swatch in the icon slot instead of an icon (a tag's color,
    /// #71): filled when the entry's state is on, a ring when off.
    swatch: Option<(String, bool)>,
    enabled: bool,
    /// The entry the menu is currently set to: drawn in the accent color,
    /// icon and label together, rather than having its icon swapped for a
    /// tick. A tick costs the icon that says what the entry *is*, which is
    /// the part worth keeping in a list of alternatives.
    selected: bool,
    activate: Box<dyn Fn()>,
    /// Sections of a nested page this entry opens instead of acting.
    submenu: Option<Vec<Vec<MenuEntry>>>,
}

impl MenuEntry {
    pub fn new(label: impl Into<String>, activate: impl Fn() + 'static) -> Self {
        Self {
            label: label.into(),
            icon: None,
            swatch: None,
            enabled: true,
            selected: false,
            activate: Box::new(activate),
            submenu: None,
        }
    }

    /// An entry that opens `sections` as a page of the same popover, headed
    /// by a back row carrying this entry's label. Disabled when empty.
    pub fn submenu(label: impl Into<String>, sections: Vec<Vec<MenuEntry>>) -> Self {
        let empty = sections.iter().all(|s| s.is_empty());
        Self {
            label: label.into(),
            icon: None,
            swatch: None,
            enabled: !empty,
            selected: false,
            activate: Box::new(|| {}),
            submenu: Some(sections),
        }
    }

    /// A colored disc in the icon slot — `on` fills it, off draws a ring —
    /// for entries that toggle something with a color of its own (tags).
    pub fn swatch(mut self, color: impl Into<String>, on: bool) -> Self {
        self.swatch = Some((color.into(), on));
        self
    }

    /// Leading symbolic icon — use the same icon as the toolbar button that
    /// performs this action, so the menu teaches the toolbar.
    pub fn icon(mut self, name: impl Into<String>) -> Self {
        self.icon = Some(name.into());
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Mark this entry as the one the menu is set to.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
}

/// Build and pop up a HIG-style context menu anchored at `(x, y)` in
/// `parent`'s own coordinate space (typically the exact widget the
/// right-click landed on, so no coordinate translation is needed). `sections`
/// groups entries into visually separated clusters, e.g. `[[Reply, Reply All,
/// Forward], [Star, Mark Read], [Spam, Archive, Delete]]`.
pub fn show_context_menu(parent: &impl IsA<gtk::Widget>, x: f64, y: f64, sections: Vec<Vec<MenuEntry>>) {
    show_context_menu_with_header(parent, x, y, None, sections);
}

/// [`show_context_menu`] with an optional dim caption header above the first
/// section (e.g. the bulk menu's "5 selected").
pub fn show_context_menu_with_header(
    parent: &impl IsA<gtk::Widget>,
    x: f64,
    y: f64,
    header: Option<&str>,
    sections: Vec<Vec<MenuEntry>>,
) {
    show_context_menu_popover(parent, x, y, header, sections);
}

/// [`show_context_menu_with_header`], handing back the popover for a caller
/// that needs to know when it closes.
pub fn show_context_menu_popover(
    parent: &impl IsA<gtk::Widget>,
    x: f64,
    y: f64,
    header: Option<&str>,
    sections: Vec<Vec<MenuEntry>>,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.add_css_class("menu");

    // Pages: the menu itself, and one per submenu, slid between. Each page
    // keeps its own size, so the popover fits whichever is showing.
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    stack.set_transition_duration(150);
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);
    stack.set_interpolate_size(true);

    let list = build_page(&popover, &stack, "main", header, sections, None);
    stack.add_named(&list, Some("main"));
    // Submenu pages were added while the main page was built; start on it.
    stack.set_visible_child_name("main");

    // Never taller than the window, scrolling past that (see below).
    let root_height = parent.as_ref().root().map(|r| r.upcast_ref::<gtk::Widget>().height());
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .child(&stack)
        .build();
    if let Some(h) = root_height.filter(|h| *h > 0) {
        scroller.set_max_content_height((h - MENU_MARGIN).max(1));
    }
    popover.set_child(Some(&scroller));
    popover.set_parent(parent);
    let y = fitted_anchor(parent.as_ref(), &popover, x, y);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|p| p.unparent());
    popup_after_release(&popover, parent.as_ref());

    // HYLKI_SHOWCASE_MENU=main|<submenu label> captures the popover's page
    // a second after it opens (the window snapshot never includes it).
    if let Ok(which) = std::env::var("HYLKI_SHOWCASE_MENU") {
        if let Ok(path) = std::env::var("HYLKI_SHOWCASE") {
            let stack = stack.clone();
            gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                if which != "main" {
                    stack.set_visible_child_name(&format!("sub:{which}"));
                }
                let stack = stack.clone();
                gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                    crate::app::showcase_capture(stack.upcast_ref(), &path);
                });
            });
        }
    }
    popover
}

/// Show `popover`, on X11 only once the mouse button that opened it is up.
///
/// Menus open on the right-click press. On X11 the release that follows
/// ends the press's implicit grab, and that breaks the grab the popover has
/// just taken: it stays on screen but no click reaches it any more, so the
/// next one only closes it and no entry ever acts. Waiting for the release
/// keeps the popover's grab whole. Wayland has no such problem, and there
/// the menu still opens on the press.
fn popup_after_release(popover: &gtk::Popover, parent: &gtk::Widget) {
    let held = gtk::gdk::ModifierType::BUTTON1_MASK
        | gtk::gdk::ModifierType::BUTTON2_MASK
        | gtk::gdk::ModifierType::BUTTON3_MASK;
    let display = parent.display();
    let is_x11 = display.type_().name() == "GdkX11Display";
    let Some(root) = parent.root().filter(|_| is_x11) else { return popover.popup() };
    // The device's own modifier state reads empty on X11; asking the
    // window's surface where the pointer is queries the server, buttons too.
    let surface = root.surface();
    let pointer = display.default_seat().and_then(|s| s.pointer());
    let state = surface.zip(pointer).and_then(|(s, p)| s.device_position(&p)).map(|(_, _, m)| m);
    let pressed = state.is_some_and(|m| m.intersects(held));
    if !pressed {
        return popover.popup();
    }

    // The press was on this window, so its release comes here too.
    let watch = gtk::EventControllerLegacy::new();
    watch.set_propagation_phase(gtk::PropagationPhase::Capture);
    let popover = popover.clone();
    watch.connect_event(move |watch, ev| {
        if ev.event_type() != gtk::gdk::EventType::ButtonRelease {
            return gtk::glib::Propagation::Proceed;
        }
        let watch = watch.clone();
        let popover = popover.clone();
        gtk::glib::idle_add_local_once(move || {
            if let Some(w) = watch.widget() {
                w.remove_controller(&watch);
            }
            popover.popup();
        });
        gtk::glib::Propagation::Proceed
    });
    root.upcast_ref::<gtk::Widget>().add_controller(watch);
}

/// Room left between a menu and the window's edges.
const MENU_MARGIN: i32 = 12;

/// Where to anchor a menu opened at `(x, y)` in `parent` so that it shows.
///
/// A popover opens below its point or, flipped, above it, but never slides
/// up or down to fit: when it fits neither way it is not shown at all. The
/// reader's message menu is taller than half a window, so a right-click
/// halfway down a message opened nothing. Here the anchor moves up just far
/// enough for the menu to fit below it, which is how a menu near the
/// bottom of a screen behaves in any GTK app.
fn fitted_anchor(parent: &gtk::Widget, popover: &gtk::Popover, x: f64, y: f64) -> f64 {
    let Some(root) = parent.root() else { return y };
    let root: &gtk::Widget = root.upcast_ref();
    let Some(at) = parent.compute_point(root, &gtk::graphene::Point::new(x as f32, y as f32)) else {
        return y;
    };
    // The popover itself measures nothing before it is mapped; its content
    // does, and the menu's padding and shadow add a little to that.
    let Some(content) = popover.child() else { return y };
    let (_, natural, _, _) = content.measure(gtk::Orientation::Vertical, -1);
    let height = natural + 2 * MENU_MARGIN;
    let (top, bottom) = (at.y() as f64, root.height() as f64);
    let fits_below = top + height as f64 <= bottom;
    let fits_above = top - (height as f64) >= 0.0;
    if fits_below || fits_above {
        return y;
    }
    let wanted = (bottom - height as f64 - MENU_MARGIN as f64).max(0.0);
    y - (top - wanted)
}

/// One page of the popover: the caption (a bulk menu's "5 selected", or a
/// submenu's back row), then the sections. A submenu entry adds its own
/// page to `stack` and slides to it; every other entry acts and closes.
fn build_page(
    popover: &gtk::Popover,
    stack: &gtk::Stack,
    page_name: &str,
    header: Option<&str>,
    sections: Vec<Vec<MenuEntry>>,
    back_to: Option<&str>,
) -> gtk::Widget {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list.add_css_class("context-menu-list");

    match (back_to, header) {
        (Some(parent_page), Some(title)) => {
            // The way back: a row with a leading chevron and the submenu's
            // name, then a hairline before its entries.
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            let img = gtk::Image::from_icon_name("go-previous-symbolic");
            img.set_pixel_size(16);
            row.append(&img);
            let lbl = gtk::Label::new(Some(title));
            lbl.set_xalign(0.0);
            lbl.set_hexpand(true);
            lbl.add_css_class("heading");
            row.append(&lbl);
            let btn = gtk::Button::new();
            btn.set_child(Some(&row));
            btn.add_css_class("flat");
            btn.add_css_class("context-menu-item");
            btn.set_halign(gtk::Align::Fill);
            let stack = stack.clone();
            let parent_page = parent_page.to_string();
            btn.connect_clicked(move |_| stack.set_visible_child_name(&parent_page));
            list.append(&btn);
            list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        (None, Some(text)) => {
            let caption = gtk::Label::new(Some(text));
            caption.set_xalign(0.0);
            caption.add_css_class("dim-label");
            caption.add_css_class("caption");
            caption.set_margin_start(10);
            caption.set_margin_top(4);
            caption.set_margin_bottom(2);
            list.append(&caption);
        }
        _ => {}
    }

    // Any icon in the menu means every row reserves the icon slot, keeping
    // the labels of iconless entries aligned with the rest.
    let has_icons = sections.iter().flatten().any(|e| e.icon.is_some() || e.swatch.is_some());

    let mut first = true;
    for entries in sections {
        if entries.is_empty() {
            continue;
        }
        if !first {
            list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        }
        first = false;

        for entry in entries {
            let MenuEntry { label, icon, swatch, enabled, selected, activate, submenu } = entry;

            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            if selected {
                row.add_css_class("context-menu-selected");
            }
            if let Some((color, on)) = swatch {
                row.append(&swatch_widget(&color, on));
            } else if has_icons {
                let img = match &icon {
                    Some(name) => gtk::Image::from_icon_name(name),
                    None => gtk::Image::new(),
                };
                img.set_pixel_size(16);
                row.append(&img);
            }
            let lbl = gtk::Label::new(Some(&label));
            lbl.set_xalign(0.0);
            lbl.set_hexpand(true);
            row.append(&lbl);

            let btn = gtk::Button::new();
            btn.set_child(Some(&row));
            btn.add_css_class("flat");
            btn.add_css_class("context-menu-item");
            btn.set_halign(gtk::Align::Fill);
            btn.set_sensitive(enabled);

            if let Some(sections) = submenu {
                // A trailing chevron says the row opens rather than acts.
                let chevron = gtk::Image::from_icon_name("pan-end-symbolic");
                chevron.set_pixel_size(16);
                chevron.add_css_class("dim-label");
                row.append(&chevron);
                let name = format!("sub:{label}");
                let page = build_page(popover, stack, &name, Some(&label), sections, Some(page_name));
                // Tall lists scroll within the page rather than past the
                // screen; short ones size exactly, as every page does.
                let scroller = gtk::ScrolledWindow::new();
                scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
                scroller.set_propagate_natural_height(true);
                scroller.set_propagate_natural_width(true);
                scroller.set_max_content_height(420);
                scroller.set_child(Some(&page));
                stack.add_named(&scroller, Some(&name));
                let stack = stack.clone();
                btn.connect_clicked(move |_| stack.set_visible_child_name(&name));
            } else {
                let weak = popover.downgrade();
                btn.connect_clicked(move |_| {
                    activate();
                    if let Some(p) = weak.upgrade() {
                        p.popdown();
                    }
                });
            }
            list.append(&btn);
        }
    }
    list.upcast()
}

/// A 16px color swatch for a menu row: a filled disc when `on`, a ring when
/// not, in `color` (`#rrggbb`; an unparsable color falls back to grey).
pub fn swatch_widget(color: &str, on: bool) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(16);
    area.set_content_height(16);
    area.set_valign(gtk::Align::Center);
    let rgba = gtk::gdk::RGBA::parse(color).unwrap_or(gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0));
    area.set_draw_func(move |_, cr, w, h| {
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        cr.set_source_rgba(
            rgba.red() as f64,
            rgba.green() as f64,
            rgba.blue() as f64,
            rgba.alpha() as f64,
        );
        if on {
            cr.arc(cx, cy, 6.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        } else {
            cr.set_line_width(2.0);
            cr.arc(cx, cy, 5.0, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        }
    });
    area
}
