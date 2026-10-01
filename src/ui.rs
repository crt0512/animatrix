use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use animatrix::{
    AppConfig, BatteryStyle, ConfigStore, DisplayProfile, Element, ElementKind, EngineCommand,
    EngineHandle, GifLayout, GifLoop, MatrixGeometry, OverlayColor, DevicePolicy, ProfileTriggers, Trigger,
    ScrollDirection, TextMode, Transition, TurnLength, WindowState,
};
use animatrix::model::{MAX_BATTERY_SCALE, MIN_BATTERY_SCALE, MAX_GIF_SCALE, MIN_GIF_SCALE, MAX_TRANSITION, MAX_BLACK_LEVEL, MAX_CONTRAST, MAX_FPS, MAX_GIF_BRIGHTNESS, MAX_OUTLINE, MAX_SCROLL_PAUSE, MAX_SPRITE_SIZE, MIN_CONTRAST};
use gtk::glib;
use gtk::prelude::*;

#[derive(Clone)]
struct UiContext {
    config: Arc<Mutex<AppConfig>>,
    store: ConfigStore,
    engine: EngineHandle,
    /// Each profile's "Active" button, so profile switches made elsewhere
    /// (tray, triggers) show up in the window.
    active_buttons: Rc<RefCell<Vec<(String, gtk::CheckButton)>>>,
    /// Profiles shown expanded. Starts with only the active one and lives
    /// for the session, so page rebuilds keep what the user opened.
    expanded: Rc<RefCell<HashSet<String>>>,
    /// Elements folded to their heading, likewise for the session.
    collapsed_elements: Rc<RefCell<HashSet<String>>>,
}

use crate::remote::APP_ID;

/// With `minimized`, the first activation opens no window; the tray or a
/// second launch opens it later.
pub fn run(config: Arc<Mutex<AppConfig>>, store: ConfigStore, engine: EngineHandle, args: &[String], minimized: bool) {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .build();
    let expanded = config.lock().ok().and_then(|config| config.active_profile.clone()).into_iter().collect();
    let context = UiContext {
        config,
        store,
        engine,
        active_buttons: Rc::default(),
        expanded: Rc::new(RefCell::new(expanded)),
        collapsed_elements: Rc::default(),
    };

    #[cfg(feature = "tray")]
    let (ui_sender, ui_receiver) = async_channel::unbounded();
    #[cfg(feature = "tray")]
    crate::tray::start(
        Arc::clone(&context.config),
        context.store.clone(),
        context.engine.clone(),
        ui_sender,
    );

    add_script_actions(&app, &context);
    let activate_context = context.clone();
    // Quitting from the tray skips the close request.
    let quit_context = context.clone();
    app.connect_shutdown(move |app| {
        for window in app.windows() {
            if let Ok(window) = window.downcast::<gtk::ApplicationWindow>() {
                remember_window_size(&quit_context, &window);
            }
        }
    });
    let start_hidden = std::cell::Cell::new(minimized);
    app.connect_startup(|_| {
        // Installed builds find the icon in hicolor; source builds use data/.
        #[cfg(debug_assertions)]
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display).add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data"));
        }
        gtk::Window::set_default_icon_name(APP_ID);
    });
    app.connect_activate(move |app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        if start_hidden.replace(false) {
            return;
        }
        build_window(app, activate_context.clone()).present();
    });

    #[cfg(feature = "tray")]
    {
        // Handled as they arrive; nothing wakes up while the tray is idle.
        let command_app = app.clone();
        glib::spawn_future_local(async move {
            while let Ok(command) = ui_receiver.recv().await {
                match command {
                    crate::tray::UiCommand::Show => command_app.activate(),
                    crate::tray::UiCommand::Quit => command_app.quit(),
                }
            }
        });
    }

    let _hold = app.hold();
    app.run_with_args(args);
    context.engine.send(EngineCommand::Shutdown);
}

/// Actions scripts reach over D-Bus (see `remote` and docs/scripting.md).
/// GLib exports them with the application; they cost nothing until called.
fn add_script_actions(app: &gtk::Application, context: &UiContext) {
    let set_profile = gtk::gio::SimpleAction::new("set-profile", Some(glib::VariantTy::STRING));
    let profile_context = context.clone();
    set_profile.connect_activate(move |_, parameter| {
        let Some(query) = parameter.and_then(|value| value.get::<String>()) else { return };
        let found = profile_context.config.lock().map_err(|error| error.to_string())
            .and_then(|config| crate::remote::find_profile(&config, &query));
        match found {
            Ok(id) => mutate_config(&profile_context, false, |config| config.active_profile = Some(id)),
            Err(error) => eprintln!("animatrix: set-profile: {error}"),
        }
    });
    let set_light_show = gtk::gio::SimpleAction::new("set-light-show", Some(glib::VariantTy::BOOLEAN));
    let light_context = context.clone();
    set_light_show.connect_activate(move |_, parameter| {
        if let Some(on) = parameter.and_then(|value| value.get::<bool>()) {
            mutate_config(&light_context, false, |config| config.enabled = on);
        }
    });
    let toggle_light_show = gtk::gio::SimpleAction::new("toggle-light-show", None);
    let toggle_context = context.clone();
    toggle_light_show.connect_activate(move |_, _| {
        mutate_config(&toggle_context, false, |config| config.enabled = !config.enabled);
    });
    app.add_action(&set_profile);
    app.add_action(&set_light_show);
    app.add_action(&toggle_light_show);
}

fn build_window(app: &gtk::Application, context: UiContext) -> gtk::ApplicationWindow {
    let saved = context.config.lock().ok().and_then(|config| config.window);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Animatrix")
        .default_width(saved.map_or(900, |state| state.width))
        .default_height(saved.map_or(680, |state| state.height))
        .maximized(saved.is_some_and(|state| state.maximized))
        .build();
    // Saved shortly after each resize, since a restart through systemd or
    // `make upgrade` kills the process without a close or quit.
    let pending_save: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let schedule_save = {
        let context = context.clone();
        move |window: &gtk::ApplicationWindow| {
            let (context, weak, pending) = (context.clone(), window.downgrade(), Rc::clone(&pending_save));
            let source = glib::timeout_add_local_once(Duration::from_millis(500), move || {
                pending.borrow_mut().take();
                if let Some(window) = weak.upgrade() {
                    remember_window_size(&context, &window);
                }
            });
            if let Some(previous) = pending_save.replace(Some(source)) {
                previous.remove();
            }
        }
    };
    let schedule_save = Rc::new(schedule_save);
    for property in ["default-width", "default-height", "maximized"] {
        let schedule_save = Rc::clone(&schedule_save);
        window.connect_notify_local(Some(property), move |window, _| schedule_save(window));
    }
    // Closing destroys the window and the next open builds a new one from
    // the saved size: GTK on X11 does not reliably keep the size or the
    // maximized state across hiding and showing a window. The app is held,
    // so it keeps running in the tray.
    let close_context = context.clone();
    window.connect_close_request(move |window| {
        remember_window_size(&close_context, window);
        glib::Propagation::Proceed
    });

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = gtk::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::builder().label("Animatrix").css_classes(["title"]).build()));
    let enabled = gtk::Switch::builder().valign(gtk::Align::Center).build();
    enabled.set_active(context.config.lock().map(|c| c.enabled).unwrap_or(false));
    header.pack_end(&enabled);
    header.pack_end(&gtk::Label::new(Some("Light show")));
    header.pack_start(&brightness_control(&context));
    let switch_context = context.clone();
    enabled.connect_active_notify(move |toggle| {
        mutate_config(&switch_context, false, |config| config.enabled = toggle.is_active());
    });
    window.set_titlebar(Some(&header));

    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    notebook.append_page(&profiles_page(context.clone()), Some(&gtk::Label::new(Some("Profiles"))));
    let device = gtk::ScrolledWindow::new();
    rebuild_device_page(&device, &context);
    notebook.append_page(&device, Some(&gtk::Label::new(Some("Device behavior"))));
    notebook.append_page(&settings_page(context.clone()), Some(&gtk::Label::new(Some("Settings"))));
    // The trigger dropdowns list profiles by name; pick up edits made on the
    // Profiles tab.
    let (device_page, device_context) = (device.clone(), context.clone());
    notebook.connect_switch_page(move |_, page, _| {
        if page == device_page.upcast_ref::<gtk::Widget>() {
            rebuild_device_page(&device_page, &device_context);
        }
    });
    root.append(&notebook);

    let preview = panel_preview(&context);
    root.append(&preview.handle);
    root.append(&preview.area);
    let status = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(12)
        .margin_end(12)
        .build();
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.append(&status);
    preview.toggle.set_valign(gtk::Align::Center);
    preview.toggle.set_margin_end(12);
    bar.append(&preview.toggle);
    root.append(&bar);
    let status_engine = context.engine.clone();
    let status_config = Arc::clone(&context.config);
    let active_buttons = Rc::clone(&context.active_buttons);
    let geometry = MatrixGeometry::detect();
    let refresh = Rc::new(move || {
        // The tray and profile triggers change these too; mirror them.
        if let Ok((on, active)) = status_config.lock().map(|config| (config.enabled, config.active_profile.clone())) {
            if enabled.is_active() != on {
                enabled.set_active(on);
            }
            for (id, button) in active_buttons.borrow().iter() {
                if Some(id) == active.as_ref() && !button.is_active() {
                    button.set_active(true);
                }
            }
        }
        if let Some(error) = status_engine.status() {
            status.set_text(&format!("Hardware error: {error}"));
            status.add_css_class("error");
        } else {
            status.set_text(&format!(
                "Ready - {:?} pixel-image canvas {}×{}",
                geometry.model, geometry.width, geometry.height
            ));
            status.remove_css_class("error");
        }
    });
    // Refresh only while the window is on screen; closing it to the tray
    // stops the timer.
    let timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let (map_timer, map_refresh) = (Rc::clone(&timer), Rc::clone(&refresh));
    let (map_preview, unmap_preview) = (preview.clone(), preview.clone());
    window.connect_map(move |_| {
        map_preview.set_running(map_preview.toggle.is_active());
        map_refresh();
        let tick = Rc::clone(&map_refresh);
        let source = glib::timeout_add_local(Duration::from_millis(500), move || {
            tick();
            glib::ControlFlow::Continue
        });
        if let Some(previous) = map_timer.replace(Some(source)) {
            previous.remove();
        }
    });
    window.connect_unmap(move |_| {
        unmap_preview.set_running(false);
        if let Some(source) = timer.take() {
            source.remove();
        }
    });

    window.set_child(Some(&root));
    window
}

fn profiles_page(context: UiContext) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    rebuild_profiles(&content, &context);
    gtk::ScrolledWindow::builder().child(&content).build()
}

/// Recreates the Profiles page from the configuration, keeping the scroll
/// position so edits far down the page do not jump back to the top.
fn rebuild_profiles(content: &gtk::Box, context: &UiContext) {
    let scroller = content.ancestor(gtk::ScrolledWindow::static_type()).and_downcast::<gtk::ScrolledWindow>();
    let position = scroller.as_ref().map(|scroller| scroller.vadjustment().value());
    while let Some(child) = content.first_child() {
        content.remove(&child);
    }
    context.active_buttons.borrow_mut().clear();

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let add = gtk::Button::with_label("Add profile");
    add.add_css_class("suggested-action");
    let add_context = context.clone();
    let add_content = content.clone();
    add.connect_clicked(move |_| {
        mutate_config(&add_context, false, |config| {
            let profile = DisplayProfile::new(&format!("Profile {}", config.profiles.len() + 1), Vec::new());
            add_context.expanded.borrow_mut().insert(profile.id.clone());
            config.active_profile = Some(profile.id.clone());
            config.profiles.push(profile);
        });
        rebuild_profiles(&add_content, &add_context);
    });
    toolbar.append(&add);
    content.append(&toolbar);

    let snapshot = context.config.lock().map(|config| config.clone()).unwrap_or_default();

    let mut active_group = None;
    let count = snapshot.profiles.len();
    for (index, profile) in snapshot.profiles.into_iter().enumerate() {
        let (card, active) = profile_card(
            context.clone(),
            content.clone(),
            profile,
            active_group.as_ref(),
            (index > 0, index + 1 < count),
        );
        if active_group.is_none() {
            active_group = Some(active);
        }
        content.append(&card);
    }

    // Restore once the new cards are laid out: GTK lays out before idle
    // callbacks run, and setting it now would clamp to the emptied page.
    if let (Some(scroller), Some(position)) = (scroller, position) {
        glib::idle_add_local_once(move || scroller.vadjustment().set_value(position));
    }
}

/// "Switch profile automatically": a dropdown and switch-back timeout per
/// power/lid change, two to a row while they fit side by side and one per
/// row when they do not.
fn triggers_section(context: &UiContext, snapshot: &AppConfig) -> gtk::Frame {
    let frame = gtk::Frame::new(Some("Switch profile automatically"));
    let flow = gtk::FlowBox::builder().min_children_per_line(1).max_children_per_line(2).homogeneous(true)
        .selection_mode(gtk::SelectionMode::None).column_spacing(12).row_spacing(12).margin_top(8)
        .margin_bottom(8).margin_start(8).margin_end(8).build();
    let mut names = vec!["Do nothing".to_owned()];
    names.extend(snapshot.profiles.iter().map(|profile| profile.name.clone()));
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let ids: Vec<String> = snapshot.profiles.iter().map(|profile| profile.id.clone()).collect();

    type Slot = fn(&mut ProfileTriggers) -> &mut Trigger;
    let rows: [(&str, Slot); 6] = [
        ("When the lid is open and plugging in", |triggers| &mut triggers.plugged_in),
        ("When the lid is open and unplugging", |triggers| &mut triggers.unplugged),
        ("When the lid closes", |triggers| &mut triggers.lid_closed),
        ("When the lid opens", |triggers| &mut triggers.lid_opened),
        ("When the lid is closed and plugging in", |triggers| &mut triggers.closed_plugged_in),
        ("When the lid is closed and unplugging", |triggers| &mut triggers.closed_unplugged),
    ];
    for (label, slot) in rows {
        let mut current = snapshot.triggers.clone();
        let trigger = slot(&mut current).clone();
        let chosen = trigger.profile.as_ref().and_then(|id| ids.iter().position(|known| known == id));
        let dropdown = gtk::DropDown::from_strings(&names);
        dropdown.set_selected(chosen.map_or(0, |index| index + 1) as u32);

        let timeout = gtk::SpinButton::with_range(0.0, 86_400.0, 1.0);
        timeout.set_value(trigger.revert_after_secs.unwrap_or(0) as f64);
        timeout.set_tooltip_text(Some(
            "Switch back to the profile that was active before after this many seconds. 0 stays on the new profile.",
        ));
        let timeout_row = labelled("Switch back after (seconds, 0 = never)", &timeout);
        timeout_row.set_sensitive(chosen.is_some());

        let item = gtk::Box::new(gtk::Orientation::Vertical, 4);
        item.append(&gtk::Label::builder().label(label).xalign(0.0).build());
        item.append(&dropdown);
        item.append(&timeout_row);
        // Only the controls inside take focus, not the flow box cell.
        let cell = gtk::FlowBoxChild::builder().child(&item).focusable(false).build();
        flow.append(&cell);

        let (trigger_context, trigger_ids, timeout_row) = (context.clone(), ids.clone(), timeout_row.clone());
        dropdown.connect_selected_notify(move |dropdown| {
            // Index 0 is "Do nothing", the rest follow the profile list.
            let id = (dropdown.selected() as usize).checked_sub(1).and_then(|index| trigger_ids.get(index).cloned());
            timeout_row.set_sensitive(id.is_some());
            mutate_config(&trigger_context, false, |config| slot(&mut config.triggers).profile = id);
        });
        let timeout_context = context.clone();
        timeout.connect_value_changed(move |spin| {
            let secs = Some(spin.value() as u32).filter(|&secs| secs > 0);
            mutate_config(&timeout_context, false, |config| slot(&mut config.triggers).revert_after_secs = secs);
        });
    }
    frame.set_child(Some(&flow));
    frame
}

fn profile_card(
    context: UiContext,
    content: gtk::Box,
    profile: DisplayProfile,
    active_group: Option<&gtk::CheckButton>,
    // Whether it can move (up, down) in the profile list.
    movable: (bool, bool),
) -> (gtk::Frame, gtk::CheckButton) {
    let frame = gtk::Frame::new(None);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    card.set_margin_top(10);
    card.set_margin_bottom(10);
    card.set_margin_start(10);
    card.set_margin_end(10);

    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    // Everything below the heading, hidden while the profile is collapsed.
    let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let is_expanded = context.expanded.borrow().contains(&profile.id);
    details.set_visible(is_expanded);
    let expander = gtk::ToggleButton::builder()
        .icon_name(if is_expanded { "pan-down-symbolic" } else { "pan-end-symbolic" })
        .tooltip_text("Show or hide this profile's settings")
        .valign(gtk::Align::Center)
        .active(is_expanded)
        .build();
    expander.add_css_class("flat");
    let (expand_details, expanded, id) = (details.clone(), Rc::clone(&context.expanded), profile.id.clone());
    expander.connect_toggled(move |button| {
        let open = button.is_active();
        expand_details.set_visible(open);
        button.set_icon_name(if open { "pan-down-symbolic" } else { "pan-end-symbolic" });
        if open { expanded.borrow_mut().insert(id.clone()); } else { expanded.borrow_mut().remove(&id); }
    });
    heading.append(&expander);
    let name = gtk::Entry::builder().text(&profile.name).hexpand(true).build();
    let id = profile.id.clone();
    let name_context = context.clone();
    name.connect_changed(move |entry| {
        let value = entry.text().to_string();
        mutate_profile(&name_context, &id, |profile| profile.name = value);
    });
    heading.append(&name);

    let active = gtk::CheckButton::with_label("Active");
    if let Some(group) = active_group {
        active.set_group(Some(group));
    }
    active.set_active(
        context.config.lock().ok().and_then(|c| c.active_profile.clone()).as_deref()
            == Some(profile.id.as_str()),
    );
    let id = profile.id.clone();
    let active_context = context.clone();
    active.connect_toggled(move |button| {
        if button.is_active() {
            mutate_config(&active_context, false, |config| config.active_profile = Some(id.clone()));
        }
    });
    heading.append(&active);
    context.active_buttons.borrow_mut().push((profile.id.clone(), active.clone()));

    // Cycling profiles can be played from their first element again.
    if profile.cycle.enabled {
        let play = gtk::Button::with_label("▶ Play from start");
        play.set_tooltip_text(Some("Show this profile's elements from the first one again, making it active if it is not"));
        let (play_context, id) = (context.clone(), profile.id.clone());
        play.connect_clicked(move |_| {
            let active = play_context.config.lock().ok().and_then(|config| config.active_profile.clone());
            if active.as_deref() != Some(id.as_str()) {
                mutate_config(&play_context, false, |config| config.active_profile = Some(id.clone()));
            }
            play_context.engine.send(EngineCommand::Restart);
        });
        heading.append(&play);
    }
    // The order is also the tray menu's.
    for (label, tooltip, step, enabled) in [("↑", "Move up", -1_isize, movable.0), ("↓", "Move down", 1, movable.1)] {
        let button = gtk::Button::with_label(label);
        button.set_tooltip_text(Some(tooltip));
        button.set_sensitive(enabled);
        let (move_context, move_content, id) = (context.clone(), content.clone(), profile.id.clone());
        button.connect_clicked(move |_| {
            mutate_config(&move_context, false, |config| {
                if let Some(from) = config.profiles.iter().position(|profile| profile.id == id) {
                    let to = from.saturating_add_signed(step);
                    if to < config.profiles.len() {
                        config.profiles.swap(from, to);
                    }
                }
            });
            rebuild_profiles(&move_content, &move_context);
        });
        heading.append(&button);
    }
    // Active last, so it sits at the edge of the heading.
    heading.reorder_child_after(&active, heading.last_child().as_ref());
    card.append(&heading);

    // Deleting sits at the bottom of the card, away from Active, and asks first.
    let delete = gtk::Button::with_label("Delete profile…");
    delete.add_css_class("destructive-action");
    let (delete_context, delete_content, id, name) = (context.clone(), content.clone(), profile.id.clone(), profile.name.clone());
    delete.connect_clicked(move |button| {
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Delete the profile “{name}”?"))
            .detail("Its elements go with it. This cannot be undone.")
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let window = button.root().and_downcast::<gtk::Window>();
        let (context, content, id) = (delete_context.clone(), delete_content.clone(), id.clone());
        dialog.choose(window.as_ref(), gtk::gio::Cancellable::NONE, move |choice| {
            if choice != Ok(1) {
                return;
            }
            mutate_config(&context, false, |config| {
                config.profiles.retain(|profile| profile.id != id);
                config.triggers.forget(&id);
                if config.active_profile.as_deref() == Some(id.as_str()) {
                    config.active_profile = config.profiles.first().map(|profile| profile.id.clone());
                }
            });
            rebuild_profiles(&content, &context);
        });
    });

    // Layered or one at a time; the cycle settings only show for the latter.
    let arrangement = gtk::DropDown::from_strings(&["Layered (all at once)", "One at a time"]);
    arrangement.set_selected(u32::from(profile.cycle.enabled));
    arrangement.set_tooltip_text(Some(
        "Layered draws every element together, the brightest wins where they overlap. One at a time cycles through them.",
    ));
    let (arrangement_context, arrangement_content, id) = (context.clone(), content.clone(), profile.id.clone());
    arrangement.connect_selected_notify(move |dropdown| {
        let enabled = dropdown.selected() == 1;
        mutate_profile(&arrangement_context, &id, |profile| profile.cycle.enabled = enabled);
        // Shows or hides each element's turn length.
        rebuild_profiles(&arrangement_content, &arrangement_context);
    });
    let arrangement_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    arrangement_row.append(&gtk::Label::new(Some("Elements:")));
    arrangement_row.append(&arrangement);
    let repeat = gtk::CheckButton::with_label("Repeat");
    repeat.set_active(profile.cycle.repeat);
    repeat.set_visible(profile.cycle.enabled);
    repeat.set_tooltip_text(Some("Start over after the last element. Unticked, the last element stays once reached."));
    let (repeat_context, id) = (context.clone(), profile.id.clone());
    repeat.connect_toggled(move |check| {
        let value = check.is_active();
        mutate_profile(&repeat_context, &id, |profile| profile.cycle.repeat = value);
    });
    arrangement_row.append(&repeat);
    details.append(&arrangement_row);


    let elements_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    elements_row.append(&gtk::Label::new(Some("Add element:")));
    for (label, constructor) in [
        ("Clock", Element::clock as fn() -> Element),
        ("Text", Element::text as fn() -> Element),
        ("GIF/Image", Element::gif as fn() -> Element),
        ("Flashlight", Element::flashlight as fn() -> Element),
        ("Battery", Element::battery as fn() -> Element),
    ] {
        let button = gtk::Button::with_label(&format!("+ {label}"));
        let add_context = context.clone();
        let add_content = content.clone();
        let id = profile.id.clone();
        button.connect_clicked(move |_| {
            mutate_profile(&add_context, &id, |profile| profile.elements.push(constructor()));
            rebuild_profiles(&add_content, &add_context);
        });
        elements_row.append(&button);
    }
    details.append(&elements_row);

    if profile.elements.is_empty() {
        details.append(&gtk::Label::builder().label("No elements yet: the display stays dark.").xalign(0.0).build());
    }
    let count = profile.elements.len();
    for (index, element) in profile.elements.iter().enumerate() {
        let target = ElementTarget { profile: profile.id.clone(), element: element.id.clone() };
        let position = (index > 0, index + 1 < count);
        details.append(&element_card(context.clone(), content.clone(), target, element.clone(), position, profile.cycle.enabled));
    }
    let duplicate = gtk::Button::with_label("Duplicate");
    duplicate.set_tooltip_text(Some("Add a copy of this profile below it, with all its elements"));
    let (copy_context, copy_content, id) = (context.clone(), content.clone(), profile.id.clone());
    duplicate.connect_clicked(move |_| {
        mutate_config(&copy_context, false, |config| {
            if let Some(index) = config.profiles.iter().position(|profile| profile.id == id) {
                let copy = config.profiles[index].duplicate();
                // Open, so its elements are right there to edit.
                copy_context.expanded.borrow_mut().insert(copy.id.clone());
                config.profiles.insert(index + 1, copy);
            }
        });
        rebuild_profiles(&copy_content, &copy_context);
    });
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_halign(gtk::Align::End);
    footer.append(&duplicate);
    footer.append(&delete);
    details.append(&footer);
    card.append(&details);
    frame.set_child(Some(&card));
    (frame, active)
}

/// Identifies one element inside one profile for UI callbacks.
#[derive(Clone)]
struct ElementTarget {
    profile: String,
    element: String,
}

/// `movable` says whether the element can move (up, down) in its profile.
fn element_card(
    context: UiContext,
    content: gtk::Box,
    target: ElementTarget,
    element: Element,
    movable: (bool, bool),
    // Whether the profile shows one element at a time, so turns matter.
    show_turn: bool,
) -> gtk::Frame {
    let frame = gtk::Frame::new(None);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    card.set_margin_top(8);
    card.set_margin_bottom(8);
    card.set_margin_start(8);
    card.set_margin_end(8);

    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    // Everything below the heading, hidden while the element is collapsed.
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let open = !context.collapsed_elements.borrow().contains(&element.id);
    body.set_visible(open);
    let expander = gtk::ToggleButton::builder()
        .icon_name(if open { "pan-down-symbolic" } else { "pan-end-symbolic" })
        .tooltip_text("Show or hide this element's settings")
        .valign(gtk::Align::Center)
        .active(open)
        .build();
    expander.add_css_class("flat");
    let (fold_body, collapsed, id) = (body.clone(), Rc::clone(&context.collapsed_elements), element.id.clone());
    expander.connect_toggled(move |button| {
        let open = button.is_active();
        fold_body.set_visible(open);
        button.set_icon_name(if open { "pan-down-symbolic" } else { "pan-end-symbolic" });
        if open { collapsed.borrow_mut().remove(&id); } else { collapsed.borrow_mut().insert(id.clone()); }
    });
    heading.append(&expander);
    heading.append(&gtk::Label::builder().label(element.kind.label()).css_classes(["heading"]).xalign(0.0).hexpand(true).build());
    if !matches!(element.kind, ElementKind::Flashlight { .. }) {
        let rotation = gtk::SpinButton::with_range(-360.0, 360.0, 1.0);
        rotation.set_value(element.rotation as f64);
        rotation.set_tooltip_text(Some("Degrees to turn this element clockwise; type any value"));
        let (rotate_context, rotate_target) = (context.clone(), target.clone());
        rotation.connect_value_changed(move |spin| {
            let value = spin.value() as f32;
            mutate_profile(&rotate_context, &rotate_target.profile, |profile| {
                if let Some(element) = profile.element_mut(&rotate_target.element) {
                    element.rotation = value;
                }
            });
        });
        heading.append(&gtk::Label::new(Some("Rotate °")));
        heading.append(&rotation);

        let tilt = gtk::CheckButton::with_label("Tilt compensation");
        tilt.set_active(element.tilt_compensation);
        tilt.set_tooltip_text(Some(
            "Lean this element against the panel's row shift so upright strokes look upright. The amount is set on the Settings tab.",
        ));
        let (tilt_context, tilt_target) = (context.clone(), target.clone());
        tilt.connect_toggled(move |check| {
            let value = check.is_active();
            mutate_profile(&tilt_context, &tilt_target.profile, |profile| {
                if let Some(element) = profile.element_mut(&tilt_target.element) {
                    element.tilt_compensation = value;
                }
            });
        });
        heading.append(&tilt);

        let smooth = gtk::CheckButton::with_label("Smooth font edges");
        smooth.set_active(element.smooth_text);
        smooth.set_tooltip_text(Some("Anti-alias text. Off lights each LED fully or not at all, which can look crisper on the panel."));
        let (smooth_context, smooth_target) = (context.clone(), target.clone());
        smooth.connect_toggled(move |check| {
            let value = check.is_active();
            mutate_profile(&smooth_context, &smooth_target.profile, |profile| {
                if let Some(element) = profile.element_mut(&smooth_target.element) {
                    element.smooth_text = value;
                }
            });
        });
        heading.append(&smooth);
    }
    // Order sets the sequence when the profile cycles its elements.
    for (label, tooltip, step, enabled) in [
        ("↑", "Move up", -1_isize, movable.0),
        ("↓", "Move down", 1, movable.1),
    ] {
        let button = gtk::Button::with_label(label);
        button.set_tooltip_text(Some(tooltip));
        button.set_sensitive(enabled);
        let (move_context, move_content, move_target) = (context.clone(), content.clone(), target.clone());
        button.connect_clicked(move |_| {
            mutate_profile(&move_context, &move_target.profile, |profile| {
                let from = profile.elements.iter().position(|element| element.id == move_target.element);
                if let Some(from) = from {
                    let to = from.saturating_add_signed(step);
                    if to < profile.elements.len() {
                        profile.elements.swap(from, to);
                    }
                }
            });
            rebuild_profiles(&move_content, &move_context);
        });
        heading.append(&button);
    }
    let duplicate = gtk::Button::with_label("Duplicate");
    duplicate.set_tooltip_text(Some("Add a copy of this element below it"));
    let (copy_context, copy_target, copy_content) = (context.clone(), target.clone(), content.clone());
    duplicate.connect_clicked(move |_| {
        mutate_profile(&copy_context, &copy_target.profile, |profile| {
            if let Some(index) = profile.elements.iter().position(|element| element.id == copy_target.element) {
                let copy = profile.elements[index].duplicate();
                profile.elements.insert(index + 1, copy);
            }
        });
        rebuild_profiles(&copy_content, &copy_context);
    });
    heading.append(&duplicate);
    let remove = gtk::Button::with_label("Remove");
    let remove_context = context.clone();
    let remove_target = target.clone();
    let remove_content = content.clone();
    remove.connect_clicked(move |_| {
        mutate_profile(&remove_context, &remove_target.profile, |profile| {
            profile.elements.retain(|element| element.id != remove_target.element);
        });
        rebuild_profiles(&remove_content, &remove_context);
    });
    heading.append(&remove);
    card.append(&heading);
    if show_turn {
        body.append(&turn_row(&context, &target, &element));
    }

    let grid = gtk::Grid::builder().column_spacing(12).row_spacing(8).build();
    match element.kind.clone() {
        ElementKind::Clock {
            font, font_size, use_24_hour, show_seconds, show_date, date_format, show_millis, fps, y_offset, x_offset,
            ignore_safe_area, date_first, millis_digits,
        } => {
            let font_entry = file_row(&grid, 0, "Font file", &font.to_string_lossy(), FileKind::Font);
            connect_element_entry(&font_entry, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { font, .. } = kind { *font = value.into(); }
            });
            let size = spin_row(&grid, 1, "Font size", MIN_FONT_SIZE, 40.0, font_size as f64);
            connect_element_spin(&size, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { font_size, .. } = kind { *font_size = value as f32; }
            });
            let hours = check_row(&grid, 2, "24-hour clock", use_24_hour);
            connect_element_check(&hours, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { use_24_hour, .. } = kind { *use_24_hour = value; }
            });
            let seconds = check_row(&grid, 3, "Show seconds", show_seconds);
            connect_element_check(&seconds, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { show_seconds, .. } = kind { *show_seconds = value; }
            });
            let date = check_row(&grid, 4, "Show date", show_date);
            connect_element_check(&date, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { show_date, .. } = kind { *show_date = value; }
            });
            let format = entry_row(&grid, 5, "Date format", &date_format);
            connect_element_entry(&format, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { date_format, .. } = kind { *date_format = value; }
            });
            let millis = check_row(&grid, 6, "Show milliseconds", show_millis);
            connect_element_check(&millis, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { show_millis, .. } = kind { *show_millis = value; }
            });
            let fps = spin_row(&grid, 7, "Milliseconds FPS", 1.0, MAX_FPS as f64, fps as f64);
            connect_element_spin(&fps, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { fps, .. } = kind { *fps = value as f32; }
            });
            offset_row(&grid, 8, (x_offset, y_offset), context.clone(), target.clone());
            let digits = spin_row(&grid, 11, "Millisecond digits", 1.0, 3.0, f64::from(millis_digits));
            connect_element_spin(&digits, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { millis_digits, .. } = kind { *millis_digits = value as u8; }
            });
            let order = check_row(&grid, 10, "Date above the time", date_first);
            connect_element_check(&order, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Clock { date_first, .. } = kind { *date_first = value; }
            });
            safe_area_row(&grid, 9, ignore_safe_area, context, target);
        }
        ElementKind::Text {
            text, font, font_size, mode, speed, fps, ignore_safe_area, scroll_pause, direction, period, y_offset, x_offset,
            limit_plays: _, plays: _, outline, invert, color,
        } => {
            let text_box = text_area_row(&grid, 0, "Text", &text);
            connect_element_text(&text_box, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { text, .. } = kind { *text = value; }
            });
            let font_entry = file_row(&grid, 1, "Font file", &font.to_string_lossy(), FileKind::Font);
            connect_element_entry(&font_entry, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { font, .. } = kind { *font = value.into(); }
            });
            let size = spin_row(&grid, 2, "Font size", MIN_FONT_SIZE, 40.0, font_size as f64);
            connect_element_spin(&size, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { font_size, .. } = kind { *font_size = value as f32; }
            });
            let labels = TextMode::ALL.map(TextMode::label);
            let selected = TextMode::ALL.iter().position(|value| *value == mode).unwrap_or(0);
            let animation = dropdown_row(&grid, 3, "Animation", &labels, selected);
            connect_element_dropdown(&animation, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Text { mode, .. }, Some(value)) = (kind, TextMode::ALL.get(index)) {
                    *mode = *value;
                }
            });
            let labels = ScrollDirection::ALL.map(ScrollDirection::label);
            let selected = ScrollDirection::ALL.iter().position(|value| *value == direction).unwrap_or(0);
            let heading = dropdown_row(&grid, 4, "Direction (scroll, bounce)", &labels, selected);
            connect_element_dropdown(&heading, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Text { direction, .. }, Some(value)) = (kind, ScrollDirection::ALL.get(index)) {
                    *direction = *value;
                }
            });
            let speed = spin_row(&grid, 5, "Pixels per second (scroll, bounce)", 1.0, 100.0, speed as f64);
            connect_element_spin(&speed, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { speed, .. } = kind { *speed = value as f32; }
            });
            let fps = spin_row(&grid, 6, "Frames per second", 1.0, MAX_FPS as f64, fps as f64);
            connect_element_spin(&fps, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { fps, .. } = kind { *fps = value as f32; }
            });
            let cycle = spin_row(&grid, 7, "Period in seconds (blink, pulse, wave; per typed character)", 0.05, 60.0, period as f64);
            cycle.set_increments(0.05, 0.5);
            cycle.set_digits(2);
            connect_element_spin(&cycle, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { period, .. } = kind { *period = value as f32; }
            });
            let pause = spin_row(&grid, 8, "Pause between passes in seconds (scroll, typewriter)", 0.0, MAX_SCROLL_PAUSE as f64, scroll_pause as f64);
            pause.set_increments(0.5, 5.0);
            pause.set_digits(1);
            connect_element_spin(&pause, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { scroll_pause, .. } = kind { *scroll_pause = value as f32; }
            });
            safe_area_row(&grid, 9, ignore_safe_area, context.clone(), target.clone());
            offset_row(&grid, 10, (x_offset, y_offset), context.clone(), target.clone());
            let border = spin_row(&grid, 12, "Outline in pixels (opposite of the text color)", 0.0, MAX_OUTLINE as f64, outline as f64);
            border.set_tooltip_text(Some("A dark border around the text so it stays readable over other elements"));
            connect_element_spin(&border, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { outline, .. } = kind { *outline = value as u32; }
            });
            let labels = OverlayColor::ALL.map(OverlayColor::label);
            let selected = OverlayColor::ALL.iter().position(|value| *value == color).unwrap_or(0);
            let tint = dropdown_row(&grid, 14, "Text color", &labels, selected);
            tint.set_tooltip_text(Some(
                "White lights LEDs, with a dark outline. Black cuts the text out of the elements beneath, with a lit outline.",
            ));
            connect_element_dropdown(&tint, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Text { color, .. }, Some(value)) = (kind, OverlayColor::ALL.get(index)) {
                    *color = *value;
                }
            });
            let inverted = check_row(&grid, 13, "Invert (white: light the panel around dark text; black: cut out all but the text)", invert);
            connect_element_check(&inverted, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Text { invert, .. } = kind { *invert = value; }
            });

        }
        ElementKind::Gif {
            path, brightness, contrast, black_level, fps, limit_plays: _, plays: _, looping, layout, size, mode, direction, speed, motion_fps, period,
            scroll_pause, ignore_safe_area, y_offset, x_offset, overlay_text, overlay_color, overlay_font, overlay_size,
            overlay_bold, overlay_italic, overlay_outline, overlay_x_offset, overlay_y_offset, smooth_scaling, overlay_rotation, overlay_mode,
            overlay_direction, overlay_speed, overlay_period, overlay_scroll_pause, invert, scale, invert_gif_only,
        } => {
            let path_entry = file_row(&grid, 0, "GIF, PNG, or JPEG file", &path.to_string_lossy(), FileKind::Gif);
            connect_element_entry(&path_entry, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { path, .. } = kind { *path = value.into(); }
            });
            let bright = spin_row(&grid, 1, "Brightness (above 1 boosts)", 0.0, MAX_GIF_BRIGHTNESS as f64, brightness as f64);
            bright.set_increments(0.05, 0.1);
            bright.set_digits(2);
            connect_element_spin(&bright, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { brightness, .. } = kind { *brightness = value as f32; }
            });
            let punch = spin_row(&grid, 2, "Contrast (1 = unchanged)", MIN_CONTRAST as f64, MAX_CONTRAST as f64, contrast as f64);
            punch.set_increments(0.05, 0.25);
            punch.set_digits(2);
            connect_element_spin(&punch, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { contrast, .. } = kind { *contrast = value as f32; }
            });
            let floor = spin_row(&grid, 3, "Black level in % (cuts dim haze)", 0.0, MAX_BLACK_LEVEL as f64, black_level as f64);
            floor.set_increments(1.0, 5.0);
            connect_element_spin(&floor, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { black_level, .. } = kind { *black_level = value as f32; }
            });
            let fps = spin_row(&grid, 4, "FPS (0 = GIF timing)", 0.0, MAX_FPS as f64, fps as f64);
            connect_element_spin(&fps, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { fps, .. } = kind { *fps = value as f32; }
            });

            let labels = GifLoop::ALL.map(GifLoop::label);
            let selected = GifLoop::ALL.iter().position(|value| *value == looping).unwrap_or(0);
            let repeat = dropdown_row(&grid, 6, "Loop", &labels, selected);
            connect_element_dropdown(&repeat, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Gif { looping, .. }, Some(value)) = (kind, GifLoop::ALL.get(index)) {
                    *looping = *value;
                }
            });

            let overlay = text_area_row(&grid, 22, "Overlay text (optional)", &overlay_text);
            connect_element_text(&overlay, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_text, .. } = kind { *overlay_text = value; }
            });
            let labels = OverlayColor::ALL.map(OverlayColor::label);
            let selected = OverlayColor::ALL.iter().position(|value| *value == overlay_color).unwrap_or(0);
            let tint = dropdown_row(&grid, 23, "Overlay color", &labels, selected);
            connect_element_dropdown(&tint, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Gif { overlay_color, .. }, Some(value)) = (kind, OverlayColor::ALL.get(index)) {
                    *overlay_color = *value;
                }
            });
            let overlay_font_entry = file_row(&grid, 24, "Overlay font file", &overlay_font.to_string_lossy(), FileKind::Font);
            connect_element_entry(&overlay_font_entry, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_font, .. } = kind { *overlay_font = value.into(); }
            });
            let overlay_scale = spin_row(&grid, 25, "Overlay font size", MIN_FONT_SIZE, 40.0, overlay_size as f64);
            connect_element_spin(&overlay_scale, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_size, .. } = kind { *overlay_size = value as f32; }
            });
            let heavy = check_row(&grid, 26, "Bold overlay", overlay_bold);
            connect_element_check(&heavy, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_bold, .. } = kind { *overlay_bold = value; }
            });
            let slanted = check_row(&grid, 27, "Italic overlay", overlay_italic);
            connect_element_check(&slanted, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_italic, .. } = kind { *overlay_italic = value; }
            });
            let border = spin_row(&grid, 28, "Overlay outline in pixels (opposite color)", 0.0, MAX_OUTLINE as f64, overlay_outline as f64);
            connect_element_spin(&border, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_outline, .. } = kind { *overlay_outline = value as u32; }
            });
            let overlay_spins = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            for (label, value, horizontal) in [("→", overlay_x_offset, true), ("↓", overlay_y_offset, false)] {
                let spin = gtk::SpinButton::with_range(-74.0, 74.0, 1.0);
                spin.set_value(value as f64);
                spin.set_tooltip_text(Some(if horizontal { "Pixels right of centre; negative moves left" } else { "Pixels below centre; negative moves up" }));
                connect_element_spin(&spin, context.clone(), target.clone(), move |kind, value| {
                    if let ElementKind::Gif { overlay_x_offset, overlay_y_offset, .. } = kind {
                        *(if horizontal { overlay_x_offset } else { overlay_y_offset }) = value as i32;
                    }
                });
                overlay_spins.append(&gtk::Label::new(Some(label)));
                overlay_spins.append(&spin);
            }
            grid.attach(&gtk::Label::builder().label("Overlay position (pixels from centre)").xalign(0.0).build(), 0, 29, 1, 1);
            grid.attach(&overlay_spins, 1, 29, 1, 1);
            // The overlay animates like a text element, stepping with the GIF.
            let labels = TextMode::ALL.map(TextMode::label);
            let selected = TextMode::ALL.iter().position(|value| *value == overlay_mode).unwrap_or(0);
            let overlay_animation = dropdown_row(&grid, 31, "Overlay animation", &labels, selected);
            overlay_animation.set_tooltip_text(Some(
                "Steps at the GIF's FPS when set, otherwise as fast as its quickest frame (at most 30 FPS)",
            ));
            connect_element_dropdown(&overlay_animation, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Gif { overlay_mode, .. }, Some(value)) = (kind, TextMode::ALL.get(index)) {
                    *overlay_mode = *value;
                }
            });
            let labels = ScrollDirection::ALL.map(ScrollDirection::label);
            let selected = ScrollDirection::ALL.iter().position(|value| *value == overlay_direction).unwrap_or(0);
            let overlay_heading = dropdown_row(&grid, 32, "Overlay direction (scroll, bounce)", &labels, selected);
            connect_element_dropdown(&overlay_heading, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Gif { overlay_direction, .. }, Some(value)) = (kind, ScrollDirection::ALL.get(index)) {
                    *overlay_direction = *value;
                }
            });
            let overlay_pace = spin_row(&grid, 33, "Overlay pixels per second (scroll, bounce)", 1.0, 100.0, overlay_speed as f64);
            connect_element_spin(&overlay_pace, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_speed, .. } = kind { *overlay_speed = value as f32; }
            });
            let overlay_cycle = spin_row(&grid, 34, "Overlay period in seconds (blink, pulse, wave; per typed character)", 0.05, 60.0, overlay_period as f64);
            overlay_cycle.set_digits(2);
            overlay_cycle.set_increments(0.05, 0.5);
            connect_element_spin(&overlay_cycle, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_period, .. } = kind { *overlay_period = value as f32; }
            });
            let overlay_pause = spin_row(&grid, 35, "Overlay pause between passes in seconds", 0.0, MAX_SCROLL_PAUSE as f64, overlay_scroll_pause as f64);
            connect_element_spin(&overlay_pause, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_scroll_pause, .. } = kind { *overlay_scroll_pause = value as f32; }
            });
            let inverted = check_row(&grid, 19, "Invert (light the dark pixels, darken the lit ones)", invert);
            connect_element_check(&inverted, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { invert, .. } = kind { *invert = value; }
            });
            let own_area = check_row(&grid, 21, "Only the GIF's own area (the rest of the panel stays dark)", invert_gif_only);
            own_area.set_margin_start(24);
            own_area.set_sensitive(invert);
            own_area.set_tooltip_text(Some(
                "Invert only where the GIF is: its rectangle, or the sprite as it moves. Unticked, the whole panel around it lights up.",
            ));
            let own_area_toggle = own_area.clone();
            inverted.connect_toggled(move |check| own_area_toggle.set_sensitive(check.is_active()));
            connect_element_check(&own_area, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { invert_gif_only, .. } = kind { *invert_gif_only = value; }
            });
            let overlay_turn = spin_row(&grid, 30, "Overlay rotation ° (clockwise)", -360.0, 360.0, overlay_rotation as f64);
            overlay_turn.set_tooltip_text(Some("Degrees to turn the overlay text about its middle; type any value"));
            connect_element_spin(&overlay_turn, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { overlay_rotation, .. } = kind { *overlay_rotation = value as f32; }
            });

            let labels = GifLayout::ALL.map(GifLayout::label);
            let selected = GifLayout::ALL.iter().position(|value| *value == layout).unwrap_or(0);
            let placement = dropdown_row(&grid, 7, "Layout", &labels, selected);
            placement.set_tooltip_text(Some(
                "Leave as is: 1:1 pixels, centred\nFit: scaled to fit, keeping proportions\n\
                 Stretch: scaled to fill the panel\nAnimate: moves like a text character",
            ));
            connect_element_dropdown(&placement, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Gif { layout, .. }, Some(value)) = (kind, GifLayout::ALL.get(index)) {
                    *layout = *value;
                }
            });
            // Animate has its own settings; rebuild so they show or hide.
            let rebuild_context = context.clone();
            let rebuild_content = content.clone();
            placement.connect_selected_notify(move |_| {
                let (context, content) = (rebuild_context.clone(), rebuild_content.clone());
                glib::idle_add_local_once(move || rebuild_profiles(&content, &context));
            });

            offset_row(&grid, 17, (x_offset, y_offset), context.clone(), target.clone());
            if layout != GifLayout::Animate {
                let factor = spin_row(&grid, 20, "Scale (× the layout's size)", f64::from(MIN_GIF_SCALE), f64::from(MAX_GIF_SCALE), f64::from(scale));
                factor.set_digits(2);
                factor.set_increments(0.05, 0.5);
                factor.set_tooltip_text(Some("1 keeps the size the layout picks; 2 doubles it, 0.5 halves it. Stays centred."));
                connect_element_spin(&factor, context.clone(), target.clone(), |kind, value| {
                    if let ElementKind::Gif { scale, .. } = kind { *scale = value as f32; }
                });
            }
            let smoothing = check_row(&grid, 18, "Smooth GIF scaling (off keeps pixel art crisp)", smooth_scaling);
            smoothing.set_tooltip_text(Some(
                "Blend pixels when the GIF is scaled (Fit, Stretch, Animate size) or turned. Off picks the nearest pixel instead.",
            ));
            connect_element_check(&smoothing, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Gif { smooth_scaling, .. } = kind { *smooth_scaling = value; }
            });
            if layout == GifLayout::Animate {
                let height = spin_row(&grid, 8, "Size (height in pixels)", 1.0, MAX_SPRITE_SIZE as f64, size as f64);
                connect_element_spin(&height, context.clone(), target.clone(), |kind, value| {
                    if let ElementKind::Gif { size, .. } = kind { *size = value as u32; }
                });
                let labels = TextMode::ALL.map(TextMode::label);
                let selected = TextMode::ALL.iter().position(|value| *value == mode).unwrap_or(0);
                let animation = dropdown_row(&grid, 9, "Animation", &labels, selected);
                connect_element_dropdown(&animation, context.clone(), target.clone(), |kind, index| {
                    if let (ElementKind::Gif { mode, .. }, Some(value)) = (kind, TextMode::ALL.get(index)) {
                        *mode = *value;
                    }
                });
                let labels = ScrollDirection::ALL.map(ScrollDirection::label);
                let selected = ScrollDirection::ALL.iter().position(|value| *value == direction).unwrap_or(0);
                let heading = dropdown_row(&grid, 10, "Direction (scroll, bounce)", &labels, selected);
                connect_element_dropdown(&heading, context.clone(), target.clone(), |kind, index| {
                    if let (ElementKind::Gif { direction, .. }, Some(value)) = (kind, ScrollDirection::ALL.get(index)) {
                        *direction = *value;
                    }
                });
                let pace = spin_row(&grid, 11, "Pixels per second (scroll, bounce)", 1.0, 100.0, speed as f64);
                connect_element_spin(&pace, context.clone(), target.clone(), |kind, value| {
                    if let ElementKind::Gif { speed, .. } = kind { *speed = value as f32; }
                });
                let redraw = spin_row(&grid, 12, "Movement FPS", 1.0, MAX_FPS as f64, motion_fps as f64);
                connect_element_spin(&redraw, context.clone(), target.clone(), |kind, value| {
                    if let ElementKind::Gif { motion_fps, .. } = kind { *motion_fps = value as f32; }
                });
                let cycle = spin_row(&grid, 13, "Period in seconds (blink, pulse, wave; per revealed column)", 0.05, 60.0, period as f64);
                cycle.set_increments(0.05, 0.5);
                cycle.set_digits(2);
                connect_element_spin(&cycle, context.clone(), target.clone(), |kind, value| {
                    if let ElementKind::Gif { period, .. } = kind { *period = value as f32; }
                });
                let pause = spin_row(&grid, 14, "Pause between passes in seconds", 0.0, MAX_SCROLL_PAUSE as f64, scroll_pause as f64);
                pause.set_increments(0.5, 5.0);
                pause.set_digits(1);
                connect_element_spin(&pause, context.clone(), target.clone(), |kind, value| {
                    if let ElementKind::Gif { scroll_pause, .. } = kind { *scroll_pause = value as f32; }
                });
                safe_area_row(&grid, 16, ignore_safe_area, context, target);
            }
        }
        ElementKind::Flashlight { brightness } => {
            let bright = spin_row(&grid, 0, "Brightness", 0.0, 1.0, brightness as f64);
            bright.set_increments(0.05, 0.1);
            bright.set_digits(2);
            connect_element_spin(&bright, context, target, |kind, value| {
                if let ElementKind::Flashlight { brightness } = kind { *brightness = value as f32; }
            });
        }
        ElementKind::Battery { font, font_size, style, label, y_offset, x_offset, ignore_safe_area, scale } => {
            let labels = BatteryStyle::ALL.map(BatteryStyle::label);
            let selected = BatteryStyle::ALL.iter().position(|value| *value == style).unwrap_or(0);
            let look = dropdown_row(&grid, 0, "Style", &labels, selected);
            connect_element_dropdown(&look, context.clone(), target.clone(), |kind, index| {
                if let (ElementKind::Battery { style, .. }, Some(value)) = (kind, BatteryStyle::ALL.get(index)) {
                    *style = *value;
                }
            });
            let caption = entry_row(&grid, 1, "Label (optional, shown below)", &label);
            connect_element_entry(&caption, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Battery { label, .. } = kind { *label = value; }
            });
            let font_entry = file_row(&grid, 2, "Font file", &font.to_string_lossy(), FileKind::Font);
            connect_element_entry(&font_entry, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Battery { font, .. } = kind { *font = value.into(); }
            });
            let size = spin_row(&grid, 3, "Font size", MIN_FONT_SIZE, 40.0, font_size as f64);
            connect_element_spin(&size, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Battery { font_size, .. } = kind { *font_size = value as f32; }
            });
            offset_row(&grid, 4, (x_offset, y_offset), context.clone(), target.clone());
            let factor = spin_row(&grid, 6, "Scale (× the whole gauge)", f64::from(MIN_BATTERY_SCALE), f64::from(MAX_BATTERY_SCALE), f64::from(scale));
            factor.set_digits(2);
            factor.set_increments(0.05, 0.25);
            factor.set_tooltip_text(Some("Grows or shrinks the gauge, percentage, and label together about their middle"));
            connect_element_spin(&factor, context.clone(), target.clone(), |kind, value| {
                if let ElementKind::Battery { scale, .. } = kind { *scale = value as f32; }
            });
            safe_area_row(&grid, 5, ignore_safe_area, context, target);
        }
    }
    body.append(&grid);
    card.append(&body);
    frame.set_child(Some(&card));
    frame
}

const BRIGHTNESS: [&str; 4] = ["off", "low", "med", "high"];

/// Header bar brightness: asusd's cap on the whole panel, under every
/// profile. Applies as soon as it changes.
fn brightness_control(context: &UiContext) -> gtk::Box {
    let current = context.config.lock().map(|config| config.policy.brightness.clone()).unwrap_or_default();
    let brightness = gtk::DropDown::from_strings(&BRIGHTNESS);
    brightness.set_selected(BRIGHTNESS.iter().position(|value| *value == current).unwrap_or(2) as u32);
    brightness.set_valign(gtk::Align::Center);
    brightness.set_tooltip_text(Some("Maximum panel brightness, for every profile"));
    let brightness_context = context.clone();
    brightness.connect_selected_notify(move |dropdown| {
        let value = BRIGHTNESS.get(dropdown.selected() as usize).unwrap_or(&"med").to_string();
        mutate_config(&brightness_context, true, |config| config.policy.brightness = value);
    });
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::Label::new(Some("Brightness")));
    row.append(&brightness);
    row
}

/// Recreates the Device behavior tab from the configuration.
fn rebuild_device_page(page: &gtk::ScrolledWindow, context: &UiContext) {
    let snapshot = context.config.lock().map(|config| config.clone()).unwrap_or_default();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&triggers_section(context, &snapshot));
    content.append(&turn_off_section(context, &snapshot.policy));
    page.set_child(Some(&content));
}

/// "Turn off when…": applied as soon as something changes.
fn turn_off_section(context: &UiContext, snapshot: &DevicePolicy) -> gtk::Frame {
    let frame = gtk::Frame::new(Some("Turn off"));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(8);
    content.set_margin_end(8);

    type Flag = fn(&mut DevicePolicy) -> &mut bool;
    let check = |label: &str, value: bool, flag: Flag| {
        let check = gtk::CheckButton::with_label(label);
        check.set_active(value);
        let check_context = context.clone();
        check.connect_toggled(move |check| {
            let value = check.is_active();
            mutate_config(&check_context, true, |config| *flag(&mut config.policy) = value);
        });
        check
    };
    content.append(&check("When unplugged", snapshot.off_when_unplugged, |policy| &mut policy.off_when_unplugged));
    content.append(&check("While suspended", snapshot.off_when_suspended, |policy| &mut policy.off_when_suspended));
    let lid = check("When the lid is closed", snapshot.off_when_lid_closed, |policy| &mut policy.off_when_lid_closed);
    content.append(&lid);

    // Lid sub-options, indented and only usable while the lid option is on.
    let lid_options = gtk::Box::new(gtk::Orientation::Vertical, 6);
    lid_options.set_margin_start(24);
    let plugged = check("Unless plugged in", snapshot.lid_stay_on_when_plugged, |policy| &mut policy.lid_stay_on_when_plugged);
    plugged.set_tooltip_text(Some("Keep the panel running with the lid closed while on mains power"));
    lid_options.append(&plugged);
    let lid_delay = gtk::SpinButton::with_range(0.0, 3600.0, 1.0);
    lid_delay.set_value(snapshot.lid_close_delay_secs as f64);
    lid_delay.set_tooltip_text(Some(
        "0 turns the display off as soon as the lid closes. With \"Unless plugged in\" this only applies on battery.",
    ));
    // Applying runs asusctl; wait for the value to settle while it is
    // being clicked or typed.
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let delay_context = context.clone();
    lid_delay.connect_value_changed(move |spin| {
        let (value, apply_context, done) = (spin.value() as u32, delay_context.clone(), Rc::clone(&pending));
        let source = glib::timeout_add_local_once(Duration::from_millis(500), move || {
            done.borrow_mut().take();
            mutate_config(&apply_context, true, |config| config.policy.lid_close_delay_secs = value);
        });
        if let Some(previous) = pending.replace(Some(source)) {
            previous.remove();
        }
    });
    lid_options.append(&labelled("Keep animating after the lid closes (seconds)", &lid_delay));
    lid_options.set_sensitive(snapshot.off_when_lid_closed);
    let lid_options_toggle = lid_options.clone();
    lid.connect_toggled(move |check| lid_options_toggle.set_sensitive(check.is_active()));
    content.append(&lid_options);
    frame.set_child(Some(&content));
    frame
}

fn settings_page(context: UiContext) -> gtk::ScrolledWindow {
    let snapshot = context.config.lock().map(|config| config.policy.clone()).unwrap_or_default();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let invert = gtk::CheckButton::with_label("Invert tray icon colors (white icon for dark panels)");
    invert.set_active(context.config.lock().map(|config| config.invert_tray_icon).unwrap_or(false));
    let invert_context = context.clone();
    invert.connect_toggled(move |check| {
        let value = check.is_active();
        mutate_config(&invert_context, false, |config| config.invert_tray_icon = value);
    });
    content.append(&invert);
    let menu_on_left = gtk::CheckButton::with_label("Swap tray clicks (left click opens the menu)");
    menu_on_left.set_tooltip_text(Some(
        "Middle click then turns the light show on or off. Right click keeps opening the menu; the panel decides that.",
    ));
    menu_on_left.set_active(context.config.lock().map(|config| config.tray_menu_on_left_click).unwrap_or(false));
    let menu_context = context.clone();
    menu_on_left.connect_toggled(move |check| {
        let value = check.is_active();
        mutate_config(&menu_context, false, |config| config.tray_menu_on_left_click = value);
    });
    content.append(&menu_on_left);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let tilt = gtk::SpinButton::with_range(-2.0, 2.0, 0.05);
    tilt.set_digits(2);
    tilt.set_value(context.config.lock().map(|config| config.tilt_per_row).unwrap_or(0.5) as f64);
    tilt.set_tooltip_text(Some(
        "How far elements with Tilt compensation lean, in pixels per row. If they look more slanted, use the opposite sign.",
    ));
    let tilt_context = context.clone();
    tilt.connect_value_changed(move |spin| {
        let value = spin.value() as f32;
        mutate_config(&tilt_context, false, |config| config.tilt_per_row = value);
    });
    content.append(&labelled("Tilt compensation (pixels per row)", &tilt));

    let row_height = gtk::SpinButton::with_range(0.5, 1.5, 0.01);
    row_height.set_digits(2);
    row_height.set_value(context.config.lock().map_or(0.65, |config| config.preview_row_height) as f64);
    row_height.set_tooltip_text(Some(
        "How tall a row of LEDs looks on the lid compared with a column's width (1 = square), for the panel preview",
    ));
    let row_context = context.clone();
    row_height.connect_value_changed(move |spin| {
        let value = spin.value() as f32;
        save_window_state(&row_context, |config| std::mem::replace(&mut config.preview_row_height, value) != value);
    });
    content.append(&labelled("Preview row height (1 = square)", &row_height));

    let profile_fade = gtk::SpinButton::with_range(0.0, f64::from(MAX_TRANSITION), 0.1);
    profile_fade.set_digits(1);
    profile_fade.set_value(context.config.lock().map_or(0.0, |config| config.profile_fade) as f64);
    profile_fade.set_tooltip_text(Some(
        "Seconds to fade the shown profile out, then the next one in, when the active profile changes. \
         An element's own fade out or (as first element) fade in is used instead where set. 0 switches at once.",
    ));
    let fade_context = context.clone();
    profile_fade.connect_value_changed(move |spin| {
        let value = spin.value() as f32;
        mutate_config(&fade_context, false, |config| config.profile_fade = value);
    });
    content.append(&labelled("Profile switch fade (seconds, 0 = off)", &profile_fade));
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    let powersave = gtk::CheckButton::with_label("Enable built-in powersave animation");
    powersave.set_active(snapshot.powersave_animation);
    content.append(&powersave);

    let boot = gtk::Entry::builder().text(&snapshot.boot_animation).build();
    let awake = gtk::Entry::builder().text(&snapshot.awake_animation).build();
    let sleep = gtk::Entry::builder().text(&snapshot.sleep_animation).build();
    let shutdown = gtk::Entry::builder().text(&snapshot.shutdown_animation).build();
    content.append(&labelled("Boot animation", &boot));
    content.append(&labelled("Awake animation", &awake));
    content.append(&labelled("Sleep animation", &sleep));
    content.append(&labelled("Shutdown animation", &shutdown));

    let apply = gtk::Button::with_label("Apply built-in animations");
    apply.add_css_class("suggested-action");
    let apply_context = context;
    apply.connect_clicked(move |_| {
        mutate_config(&apply_context, true, |config| {
            config.policy.powersave_animation = powersave.is_active();
            config.policy.boot_animation = boot.text().to_string();
            config.policy.awake_animation = awake.text().to_string();
            config.policy.sleep_animation = sleep.text().to_string();
            config.policy.shutdown_animation = shutdown.text().to_string();
        });
    });
    content.append(&apply);
    gtk::ScrolledWindow::builder().child(&content).build()
}

fn entry_row(grid: &gtk::Grid, row: i32, label: &str, value: &str) -> gtk::Entry {
    let entry = gtk::Entry::builder().text(value).hexpand(true).build();
    grid.attach(&gtk::Label::builder().label(label).xalign(0.0).build(), 0, row, 1, 1);
    grid.attach(&entry, 1, row, 1, 1);
    entry
}

/// What a path entry expects, for the file chooser's filter and start folder.
#[derive(Clone, Copy)]
enum FileKind {
    Font,
    Gif,
}

/// A path entry with a "Browse…" button. The chosen file is written into the
/// entry, which saves it like typing does.
fn file_row(grid: &gtk::Grid, row: i32, label: &str, value: &str, kind: FileKind) -> gtk::Entry {
    let entry = entry_row(grid, row, label, value);
    let browse = gtk::Button::with_label("Browse…");
    grid.attach(&browse, 2, row, 1, 1);
    let target = entry.clone();
    browse.connect_clicked(move |button| {
        let filter = gtk::FileFilter::new();
        let current = PathBuf::from(target.text().as_str());
        let (title, current, fallback) = match kind {
            FileKind::Font => {
                filter.set_name(Some("Fonts"));
                for pattern in ["*.ttf", "*.otf", "*.TTF", "*.OTF"] {
                    filter.add_pattern(pattern);
                }
                ("Choose a font", Some(current), PathBuf::from("/usr/share/fonts"))
            }
            FileKind::Gif => {
                filter.set_name(Some("GIFs and images"));
                for pattern in ["*.gif", "*.GIF", "*.png", "*.PNG", "*.jpg", "*.JPG", "*.jpeg", "*.JPEG"] {
                    filter.add_pattern(pattern);
                }
                let bundled = animatrix::assets::gif_dirs().into_iter().next();
                let fallback = bundled.or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_default();
                ("Choose a GIF or image", animatrix::assets::resolve_gif(&current), fallback)
            }
        };
        // Start next to the current file when there is one.
        let folder = current
            .as_deref()
            .and_then(Path::parent)
            .filter(|folder| folder.is_dir())
            .map_or(fallback, Path::to_path_buf);
        let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title(title)
            .modal(true)
            .filters(&filters)
            .default_filter(&filter)
            .initial_folder(&gtk::gio::File::for_path(folder))
            .build();
        let window = button.root().and_downcast::<gtk::Window>();
        let entry = target.clone();
        dialog.open(window.as_ref(), None::<&gtk::gio::Cancellable>, move |result| {
            // Cancelling reports an error; only a chosen file changes anything.
            if let Some(path) = result.ok().and_then(|file| file.path()) {
                entry.set_text(&path.to_string_lossy());
            }
        });
    });
    entry
}

fn spin_row(grid: &gtk::Grid, row: i32, label: &str, min: f64, max: f64, value: f64) -> gtk::SpinButton {
    let spin = gtk::SpinButton::with_range(min, max, 1.0);
    spin.set_value(value);
    grid.attach(&gtk::Label::builder().label(label).xalign(0.0).build(), 0, row, 1, 1);
    grid.attach(&spin, 1, row, 1, 1);
    spin
}

fn dropdown_row(grid: &gtk::Grid, row: i32, label: &str, options: &[&str], selected: usize) -> gtk::DropDown {
    let dropdown = gtk::DropDown::from_strings(options);
    dropdown.set_selected(selected as u32);
    grid.attach(&gtk::Label::builder().label(label).xalign(0.0).build(), 0, row, 1, 1);
    grid.attach(&dropdown, 1, row, 1, 1);
    dropdown
}

/// "Shown for N seconds / cycles": how long the element stays when its
/// profile shows one element at a time. Only text and GIFs have animation
/// cycles to count.
/// Below it, the element's fades and the pause after it.
fn turn_row(context: &UiContext, target: &ElementTarget, element: &Element) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let turn = element.turn.unwrap_or_default();
    let (count, cycles) = match turn {
        TurnLength::Seconds(seconds) => (seconds, false),
        TurnLength::Cycles(plays) => (plays, true),
    };
    let amount = gtk::SpinButton::with_range(1.0, 3600.0, 1.0);
    amount.set_value(f64::from(count));
    let can_cycle = matches!(element.kind, ElementKind::Text { .. } | ElementKind::Gif { .. });
    let unit = gtk::DropDown::from_strings(&["seconds", "cycles"]);
    unit.set_selected(u32::from(cycles && can_cycle));
    unit.set_tooltip_text(Some(
        "Cycles: complete plays of the animation (a scroll pass, a GIF loop, …). Static text and still images count one second per cycle.",
    ));
    // Read both widgets, so either change saves the whole turn.
    let save = {
        let (context, target, amount, unit) = (context.clone(), target.clone(), amount.clone(), unit.clone());
        move || {
            let count = amount.value() as u32;
            let turn = if unit.selected() == 1 { TurnLength::Cycles(count) } else { TurnLength::Seconds(count) };
            mutate_profile(&context, &target.profile, |profile| {
                if let Some(element) = profile.element_mut(&target.element) {
                    element.turn = Some(turn);
                }
            });
        }
    };
    let save = Rc::new(save);
    let on_amount = Rc::clone(&save);
    amount.connect_value_changed(move |_| on_amount());
    unit.connect_selected_notify(move |_| save());
    row.append(&gtk::Label::new(Some("Shown for")));
    row.append(&amount);
    if can_cycle {
        row.append(&unit);
    } else {
        row.append(&gtk::Label::new(Some("seconds")));
    }

    // Fades within the turn, then a dark pause before the next element.
    let transition_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    type Field = fn(&mut Transition) -> &mut f32;
    let fields: [(&str, &str, f32, Field); 3] = [
        ("fade in", "Seconds to brighten from dark at the start of its turn", element.transition.fade_in, |t| &mut t.fade_in),
        ("fade out", "Seconds to dim to dark at the end of its turn", element.transition.fade_out, |t| &mut t.fade_out),
        ("pause after", "Seconds of dark panel before the next element", element.transition.pause, |t| &mut t.pause),
    ];
    for (label, tooltip, value, field) in fields {
        let spin = gtk::SpinButton::with_range(0.0, f64::from(MAX_TRANSITION), 0.1);
        spin.set_digits(1);
        spin.set_value(f64::from(value));
        spin.set_tooltip_text(Some(tooltip));
        let (context, target) = (context.clone(), target.clone());
        spin.connect_value_changed(move |spin| {
            let value = spin.value() as f32;
            mutate_profile(&context, &target.profile, |profile| {
                if let Some(element) = profile.element_mut(&target.element) {
                    *field(&mut element.transition) = value;
                }
            });
        });
        transition_row.append(&gtk::Label::new(Some(label)));
        transition_row.append(&spin);
    }
    transition_row.append(&gtk::Label::new(Some("seconds")));

    let rows = gtk::Box::new(gtk::Orientation::Vertical, 6);
    rows.append(&row);
    rows.append(&transition_row);
    rows
}

/// "Ignore safe area" checkbox shared by every element that lays out content.
fn safe_area_row(grid: &gtk::Grid, row: i32, value: bool, context: UiContext, target: ElementTarget) {
    let check = check_row(grid, row, "Ignore safe area (may clip at the panel edges)", value);
    check.set_tooltip_text(Some(
        "Lay out on the whole canvas. Clocks and text then scale to fill it and ignore the font size; \
         unticked, they keep the font size, centred on the guaranteed-visible area.",
    ));
    connect_element_check(&check, context, target, |kind, value| {
        if let ElementKind::Clock { ignore_safe_area, .. }
        | ElementKind::Text { ignore_safe_area, .. }
        | ElementKind::Battery { ignore_safe_area, .. }
        | ElementKind::Gif { ignore_safe_area, .. } = kind
        {
            *ignore_safe_area = value;
        }
    });
}

/// Smallest font size the editors offer; tiny sizes suit pixel fonts.
const MIN_FONT_SIZE: f64 = 1.0;

/// Offset editor shared by the text-like elements: right and down, in
/// pixels.
fn offset_row(grid: &gtk::Grid, row: i32, (x, y): (i32, i32), context: UiContext, target: ElementTarget) {
    let spins = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    for (label, value, horizontal) in [("→", x, true), ("↓", y, false)] {
        let spin = gtk::SpinButton::with_range(-74.0, 74.0, 1.0);
        spin.set_value(value as f64);
        spin.set_tooltip_text(Some(if horizontal { "Pixels right; negative moves left" } else { "Pixels down; negative moves up" }));
        connect_element_spin(&spin, context.clone(), target.clone(), move |kind, value| {
            if let ElementKind::Clock { x_offset, y_offset, .. }
            | ElementKind::Text { x_offset, y_offset, .. }
            | ElementKind::Battery { x_offset, y_offset, .. }
            | ElementKind::Gif { x_offset, y_offset, .. } = kind
            {
                *(if horizontal { x_offset } else { y_offset }) = value as i32;
            }
        });
        spins.append(&gtk::Label::new(Some(label)));
        spins.append(&spin);
    }
    grid.attach(&gtk::Label::builder().label("Offset (pixels)").xalign(0.0).build(), 0, row, 1, 1);
    grid.attach(&spins, 1, row, 1, 1);
}

fn check_row(grid: &gtk::Grid, row: i32, label: &str, value: bool) -> gtk::CheckButton {
    let check = gtk::CheckButton::with_label(label);
    check.set_active(value);
    grid.attach(&check, 0, row, 2, 1);
    check
}

fn labelled<W: IsA<gtk::Widget>>(text: &str, widget: &W) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.append(&gtk::Label::builder().label(text).xalign(0.0).hexpand(true).build());
    row.append(widget);
    row
}

/// A multi-line text box: Enter starts a new line, and it grows with them.
fn text_area_row(grid: &gtk::Grid, row: i32, label: &str, value: &str) -> gtk::TextBuffer {
    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(value);
    let view = gtk::TextView::builder()
        .buffer(&buffer)
        .accepts_tab(false)
        .hexpand(true)
        .top_margin(6)
        .bottom_margin(6)
        .left_margin(8)
        .right_margin(8)
        .build();
    let frame = gtk::Frame::builder().child(&view).build();
    grid.attach(&gtk::Label::builder().label(label).xalign(0.0).valign(gtk::Align::Start).margin_top(6).build(), 0, row, 1, 1);
    grid.attach(&frame, 1, row, 1, 1);
    buffer
}

fn connect_element_text<F>(buffer: &gtk::TextBuffer, context: UiContext, target: ElementTarget, update: F)
where
    F: Fn(&mut ElementKind, String) + 'static,
{
    buffer.connect_changed(move |buffer| {
        let value = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
        mutate_element(&context, &target, |kind| update(kind, value));
    });
}

fn connect_element_entry<F>(entry: &gtk::Entry, context: UiContext, target: ElementTarget, update: F)
where
    F: Fn(&mut ElementKind, String) + 'static,
{
    entry.connect_changed(move |entry| {
        let value = entry.text().to_string();
        mutate_element(&context, &target, |kind| update(kind, value));
    });
}

fn connect_element_spin<F>(spin: &gtk::SpinButton, context: UiContext, target: ElementTarget, update: F)
where
    F: Fn(&mut ElementKind, f64) + 'static,
{
    spin.connect_value_changed(move |spin| {
        mutate_element(&context, &target, |kind| update(kind, spin.value()));
    });
}

fn connect_element_dropdown<F>(dropdown: &gtk::DropDown, context: UiContext, target: ElementTarget, update: F)
where
    F: Fn(&mut ElementKind, usize) + 'static,
{
    dropdown.connect_selected_notify(move |dropdown| {
        let index = dropdown.selected() as usize;
        mutate_element(&context, &target, |kind| update(kind, index));
    });
}

fn connect_element_check<F>(check: &gtk::CheckButton, context: UiContext, target: ElementTarget, update: F)
where
    F: Fn(&mut ElementKind, bool) + 'static,
{
    check.connect_toggled(move |check| {
        mutate_element(&context, &target, |kind| update(kind, check.is_active()));
    });
}

fn mutate_element<F>(context: &UiContext, target: &ElementTarget, update: F)
where
    F: FnOnce(&mut ElementKind),
{
    mutate_profile(context, &target.profile, |profile| {
        if let Some(element) = profile.element_mut(&target.element) {
            update(&mut element.kind);
        }
    });
}

fn mutate_profile<F>(context: &UiContext, id: &str, update: F)
where
    F: FnOnce(&mut DisplayProfile),
{
    mutate_config(context, false, |config| {
        if let Some(profile) = config.profiles.iter_mut().find(|profile| profile.id == id) {
            update(profile);
        }
    });
}

/// Saves the window size without refreshing the engine; nothing on the
/// panel depends on it.
fn remember_window_size(context: &UiContext, window: &gtk::ApplicationWindow) {
    // While maximized, the default size still holds the unmaximized one.
    let (width, height) = window.default_size();
    let state = WindowState { width, height, maximized: window.is_maximized() };
    save_window_state(context, |config| config.window.replace(state) != Some(state));
}

/// Saves window-only settings (size, preview) without refreshing the
/// engine; nothing on the panel depends on them. `update` says whether it
/// changed anything.
fn save_window_state(context: &UiContext, update: impl FnOnce(&mut AppConfig) -> bool) {
    let snapshot = match context.config.lock() {
        Ok(mut config) => {
            if !update(&mut config) {
                return;
            }
            config.clone()
        }
        Err(_) => return,
    };
    if let Err(error) = context.store.save(&snapshot) {
        eprintln!("animatrix: failed to save window state: {error:#}");
    }
}

/// The panel preview in the bottom bar: a drag handle and a drawing of the
/// LEDs above the status line, and the toggle that shows them.
#[derive(Clone)]
struct PanelPreview {
    handle: gtk::Box,
    area: gtk::DrawingArea,
    toggle: gtk::ToggleButton,
    /// Polls the engine for new frames while the preview is on screen.
    timer: Rc<RefCell<Option<glib::SourceId>>>,
    engine: EngineHandle,
    frame: Rc<RefCell<animatrix::PanelFrame>>,
    /// The row height last drawn with; the setting is checked while polling.
    row_height: Rc<std::cell::Cell<f32>>,
    config: Arc<Mutex<AppConfig>>,
}

impl PanelPreview {
    const MIN_HEIGHT: i32 = 60;
    const MAX_HEIGHT: i32 = 480;
    /// About the panel's own top speed; nothing is redrawn without a new frame.
    const POLL: Duration = Duration::from_millis(33);

    fn set_running(&self, running: bool) {
        if let Some(source) = self.timer.take() {
            source.remove();
        }
        if !running {
            return;
        }
        let preview = self.clone();
        let poll = move || {
            let seen = preview.frame.borrow().serial;
            if let Some(frame) = preview.engine.frame_after(seen) {
                *preview.frame.borrow_mut() = frame;
                preview.area.queue_draw();
            }
            // Picks up a change on the Settings tab while the panel is still.
            if let Ok(row_height) = preview.config.lock().map(|config| config.preview_row_height)
                && preview.row_height.replace(row_height) != row_height
            {
                preview.area.queue_draw();
            }
        };
        poll();
        self.timer.replace(Some(glib::timeout_add_local(Self::POLL, move || {
            poll();
            glib::ControlFlow::Continue
        })));
    }

    /// The tallest the preview may get: its limit, and at most most of the
    /// window so the tabs stay usable.
    fn max_height(&self) -> i32 {
        let window = self.area.root().map_or(0, |root| root.height());
        if window > 0 { Self::MAX_HEIGHT.min(window * 3 / 5).max(Self::MIN_HEIGHT) } else { Self::MAX_HEIGHT }
    }
}

fn panel_preview(context: &UiContext) -> PanelPreview {
    let (shown, height) = context.config.lock().map(|config| (config.preview_shown, config.preview_height)).unwrap_or((false, 160));
    let area = gtk::DrawingArea::builder()
        .content_height(height.clamp(PanelPreview::MIN_HEIGHT, PanelPreview::MAX_HEIGHT))
        .visible(shown)
        .build();
    let handle = gtk::Box::builder().height_request(8).visible(shown).tooltip_text("Drag to resize the preview").build();
    handle.append(&gtk::Separator::builder().hexpand(true).valign(gtk::Align::Center).build());
    handle.set_cursor_from_name(Some("row-resize"));
    let toggle = gtk::ToggleButton::builder().label("Preview").active(shown).tooltip_text("Show what the panel is displaying").build();
    let preview = PanelPreview {
        handle,
        area,
        toggle,
        timer: Rc::default(),
        engine: context.engine.clone(),
        frame: Rc::default(),
        row_height: Rc::new(std::cell::Cell::new(context.config.lock().map_or(0.65, |config| config.preview_row_height))),
        config: Arc::clone(&context.config),
    };

    // LEDs drawn where they sit on the canvas, so the drawing has the
    // panel's shape, turned the way the panel sits on the lid (short
    // straight edge along the top on the GA402); unlit ones stay faintly
    // visible. Rows on the lid sit closer together than columns, so the
    // turned drawing is squashed by the Settings tab's row height.
    const TURN_DEGREES: f64 = 45.0;
    let geometry = MatrixGeometry::detect();
    let (sin, cos) = TURN_DEGREES.to_radians().sin_cos();
    let turned: Vec<Option<(f64, f64)>> = animatrix::matrix::led_positions(geometry)
        .into_iter()
        .map(|position| position.map(|(x, y)| (f64::from(x), f64::from(y))).map(|(x, y)| (x * cos - y * sin, x * sin + y * cos)))
        .collect();
    let (mut min, mut max) = ((f64::MAX, f64::MAX), (f64::MIN, f64::MIN));
    for &(x, y) in turned.iter().flatten() {
        (min, max) = ((min.0.min(x), min.1.min(y)), (max.0.max(x), max.1.max(y)));
    }
    let (frame, row_height) = (Rc::clone(&preview.frame), Rc::clone(&preview.row_height));
    preview.area.set_draw_func(move |_, cairo, width, height| {
        cairo.set_source_rgb(0.04, 0.04, 0.05);
        let _ = cairo.paint();
        let squash = f64::from(row_height.get());
        // One cell of room around the outermost LED centres.
        let (columns, rows) = (max.0 - min.0 + 1.0, (max.1 - min.1) * squash + 1.0);
        let cell = (f64::from(width) / columns).min(f64::from(height) / rows);
        let left = (f64::from(width) - cell * columns) / 2.0;
        let top = (f64::from(height) - cell * rows) / 2.0;
        // Squashed, neighbours can be under a cell apart.
        let radius = (cell * 0.45 * squash.min(1.0)).max(0.6);
        let frame = frame.borrow();
        for (index, position) in turned.iter().enumerate() {
            let Some((x, y)) = *position else { continue };
            let level = f64::from(frame.leds.get(index).copied().unwrap_or(0)) / 255.0;
            let shade = 0.13 + 0.87 * level;
            cairo.set_source_rgb(shade, shade, shade * 0.97);
            let (cx, cy) = (left + (x - min.0 + 0.5) * cell, top + ((y - min.1) * squash + 0.5) * cell);
            cairo.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            let _ = cairo.fill();
        }
    });

    // Dragging the handle up makes the preview taller, within its limits.
    let drag = gtk::GestureDrag::new();
    let start_height = Rc::new(std::cell::Cell::new(0));
    let (begin_preview, begin_start) = (preview.clone(), Rc::clone(&start_height));
    drag.connect_drag_begin(move |_, _, _| begin_start.set(begin_preview.area.content_height()));
    let (update_preview, update_start) = (preview.clone(), Rc::clone(&start_height));
    drag.connect_drag_update(move |_, _, dy| {
        let height = (update_start.get() - dy as i32).clamp(PanelPreview::MIN_HEIGHT, update_preview.max_height());
        update_preview.area.set_content_height(height);
    });
    let (end_preview, end_context) = (preview.clone(), context.clone());
    drag.connect_drag_end(move |_, _, _| {
        let height = end_preview.area.content_height();
        save_window_state(&end_context, |config| std::mem::replace(&mut config.preview_height, height) != height);
    });
    preview.handle.add_controller(drag);

    let (toggle_preview, toggle_context) = (preview.clone(), context.clone());
    preview.toggle.connect_toggled(move |toggle| {
        let shown = toggle.is_active();
        toggle_preview.area.set_visible(shown);
        toggle_preview.handle.set_visible(shown);
        toggle_preview.set_running(shown);
        save_window_state(&toggle_context, |config| std::mem::replace(&mut config.preview_shown, shown) != shown);
    });
    preview
}

fn mutate_config<F>(context: &UiContext, policy_changed: bool, update: F)
where
    F: FnOnce(&mut AppConfig),
{
    let snapshot = match context.config.lock() {
        Ok(mut config) => {
            update(&mut config);
            config.clone()
        }
        Err(error) => {
            eprintln!("animatrix: configuration lock failed: {error}");
            return;
        }
    };
    if let Err(error) = context.store.save(&snapshot) {
        eprintln!("animatrix: failed to save configuration: {error:#}");
    }
    if policy_changed { context.engine.apply_policy(); } else { context.engine.refresh(); }
}
