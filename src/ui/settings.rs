//! # HYLKI THEME SETTINGS UI & 10-SECOND SAFETY ROLLBACK
//!
//! G'day mate! This module builds the GTK dropdown selector for external .theme bundles
//! and enforces the non-negotiable 10-second safety rollback dialog.
//! If the user doesn't confirm the new look within 10 seconds, she rolls right back to
//! the previous style cleanly with zero fuss!
//!
//! STRICT DIRECTIVES ENFORCED:
//! 1. Strictly selection UI only (NO drag-and-drop targets allowed).
//! 2. Mandatory 10-second rollback with glib timer timeout.
//! 3. CRITICAL UI CONSTRAINT: Non-modal dialog (`set_modal(false)`) so the main window
//!    is NOT dimmed, blacked out, or obscured during theme evaluation.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use adw::prelude::*;
use libadwaita::prelude::MessageDialogExt;
use gettextrs::gettext as i18n;
use gtk::glib;

use crate::theme_manager::{self, ThemeInfo};

/// Builds the custom .theme selector dropdown with safety rollback confirmation.
/// Strictly NO drag & drop — dropdown selection only as mandated by directives, mate!
pub fn build_theme_dropdown(parent_win: Option<&gtk::Window>) -> gtk::DropDown {
    let themes = theme_manager::discover_themes();

    // Index 0 is always "Default (Stock Look)"
    let mut labels = vec![i18n("Default (Stock Look)")];
    let mut theme_ids: Vec<Option<String>> = vec![None];

    for t in &themes {
        labels.push(t.name.clone());
        theme_ids.push(Some(t.id.clone()));
    }

    let string_list = gtk::StringList::new(&labels.iter().map(|s| s.as_str()).collect::<Vec<&str>>());
    let dropdown = gtk::DropDown::new(Some(string_list), gtk::Expression::NONE);

    // Initial selected index based on active theme
    let current_id = theme_manager::active_theme_id();
    let initial_idx = theme_ids
        .iter()
        .position(|id_opt| *id_opt == current_id)
        .unwrap_or(0);
    dropdown.set_selected(initial_idx as u32);

    // Track last confirmed index to avoid false change events or rollback loops
    let confirmed_index = Rc::new(RefCell::new(initial_idx as u32));
    let is_reverting = Rc::new(RefCell::new(false));

    let parent_win_opt = parent_win.cloned();
    let theme_ids_rc = Rc::new(theme_ids);

    dropdown.connect_selected_notify(move |dd| {
        // Guard against programmatic index reverts triggering the safety dialog again
        if *is_reverting.borrow() {
            return;
        }

        let new_idx = dd.selected();
        let prev_idx = *confirmed_index.borrow();
        if new_idx == prev_idx {
            return;
        }

        let target_theme_id = theme_ids_rc.get(new_idx as usize).cloned().flatten();

        // 1. Prepare switch (saves backup provider, window transparency, and reader CSS)
        theme_manager::prepare_switch();

        // 2. Apply target theme immediately for visual preview
        if let Err(e) = theme_manager::apply_theme(target_theme_id.as_deref()) {
            tracing::error!("[settings] Strewth! Failed to apply theme: {}", e);
            theme_manager::rollback();
            *is_reverting.borrow_mut() = true;
            dd.set_selected(prev_idx);
            *is_reverting.borrow_mut() = false;
            return;
        }

        // 3. Mandatory 10-Second Safety Rollback Confirmation Dialog
        let dialog = adw::MessageDialog::new(
            parent_win_opt.as_ref(),
            Some(&i18n("Keep this theme?")),
            Some(&i18n("Reverting to previous theme in 10 seconds...")),
        );

        // CRITICAL UI CONSTRAINT: Explicitly set popup to be non-modal (set_modal(false))
        // so it does NOT blackout, dim, or darken the main application window!
        // The user must have a clear, unobstructed view of the new theme to evaluate it, mate!
        dialog.set_modal(false);

        dialog.add_response("revert", &i18n("Revert Now"));
        dialog.add_response("keep", &i18n("Keep Theme"));
        dialog.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
        dialog.set_response_appearance("revert", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("revert");

        let timer_seconds = Rc::new(RefCell::new(10));
        let dialog_closed = Rc::new(RefCell::new(false));

        // Ticking countdown timer
        let timer_dialog = dialog.clone();
        let timer_closed = dialog_closed.clone();
        let timer_secs = timer_seconds.clone();

        glib::timeout_add_seconds_local(1, move || {
            if *timer_closed.borrow() {
                return glib::ControlFlow::Break;
            }

            let mut secs = timer_secs.borrow_mut();
            if *secs > 1 {
                *secs -= 1;
                let body_text = format!("Reverting to previous theme in {} seconds...", *secs);
                timer_dialog.set_body(&body_text);
                glib::ControlFlow::Continue
            } else {
                // Time is up! Execute automatic rollback, fair dinkum!
                *timer_closed.borrow_mut() = true;
                timer_dialog.response("revert");
                glib::ControlFlow::Break
            }
        });

        // Dialog response handler
        let dd_clone = dd.clone();
        let is_reverting_clone = is_reverting.clone();
        let confirmed_index_clone = confirmed_index.clone();
        let dialog_closed_clone = dialog_closed.clone();

        dialog.connect_response(move |d, response| {
            *dialog_closed_clone.borrow_mut() = true;
            if response == "keep" {
                // User confirmed: commit choice permanently
                theme_manager::commit();
                *confirmed_index_clone.borrow_mut() = new_idx;
                tracing::info!("[settings] User confirmed theme switch to index {}. She'll be right!", new_idx);
            } else {
                // Revert response or timeout expiry: Rollback style provider & reset dropdown
                theme_manager::rollback();
                *is_reverting_clone.borrow_mut() = true;
                dd_clone.set_selected(prev_idx);
                *is_reverting_clone.borrow_mut() = false;
                tracing::info!("[settings] Theme safety rollback executed; reverted to index {}, mate.", prev_idx);
            }
            d.destroy();
        });

        dialog.present();
    });

    dropdown
}

/// Temporary toggle / cycler to quickly view and cycle through themes with the 10s rollback safety dialog.
/// She's a ripper for rapid testing across Retro Terminal, Outlook 365, Apple Mail, and Glass Vibrancy!
pub fn cycle_next_theme(parent_win: Option<&gtk::Window>) {
    let themes = theme_manager::discover_themes();
    let current_id = theme_manager::active_theme_id();

    // Cycle sequence: Default (None) -> themes[0] -> themes[1] ... -> Default (None)
    let next_theme_id = match current_id {
        None => themes.first().map(|t| t.id.clone()),
        Some(curr) => {
            if let Some(pos) = themes.iter().position(|t| t.id == curr) {
                if pos + 1 < themes.len() {
                    Some(themes[pos + 1].id.clone())
                } else {
                    None // Wrap back around to default stock look
                }
            } else {
                themes.first().map(|t| t.id.clone())
            }
        }
    };

    theme_manager::prepare_switch();
    if let Err(e) = theme_manager::apply_theme(next_theme_id.as_deref()) {
        tracing::error!("[settings] Strewth! Failed to cycle theme: {}", e);
        theme_manager::rollback();
        return;
    }

    let target_name = next_theme_id
        .as_deref()
        .and_then(|id| themes.iter().find(|t| t.id == id).map(|t| t.name.as_str()))
        .unwrap_or("Default (Stock Look)");

    let dialog = adw::MessageDialog::new(
        parent_win,
        Some(&format!("Keep theme '{}'?", target_name)),
        Some(&i18n("Reverting to previous theme in 10 seconds...")),
    );

    // CRITICAL UI CONSTRAINT: Explicitly non-modal to keep the background bright and clear, she'll be right!
    dialog.set_modal(false);

    dialog.add_response("revert", &i18n("Revert Now"));
    dialog.add_response("keep", &i18n("Keep Theme"));
    dialog.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
    dialog.set_response_appearance("revert", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    dialog.set_close_response("revert");

    let timer_seconds = Rc::new(RefCell::new(10));
    let dialog_closed = Rc::new(RefCell::new(false));

    let timer_dialog = dialog.clone();
    let timer_closed = dialog_closed.clone();
    let timer_secs = timer_seconds.clone();

    glib::timeout_add_seconds_local(1, move || {
        if *timer_closed.borrow() {
            return glib::ControlFlow::Break;
        }
        let mut secs = timer_secs.borrow_mut();
        if *secs > 1 {
            *secs -= 1;
            let body_text = format!("Reverting to previous theme in {} seconds...", *secs);
            timer_dialog.set_body(&body_text);
            glib::ControlFlow::Continue
        } else {
            *timer_closed.borrow_mut() = true;
            timer_dialog.response("revert");
            glib::ControlFlow::Break
        }
    });

    let dialog_closed_clone = dialog_closed.clone();
    dialog.connect_response(move |d, response| {
        *dialog_closed_clone.borrow_mut() = true;
        if response == "keep" {
            theme_manager::commit();
            tracing::info!("[settings] User confirmed cycled theme. Good on ya, mate!");
        } else {
            theme_manager::rollback();
            tracing::info!("[settings] Cycled theme rollback executed. Back to safety!");
        }
        d.destroy();
    });

    dialog.present();
}
