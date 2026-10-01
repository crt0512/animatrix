use std::sync::{Arc, Mutex};

use animatrix::{AppConfig, ConfigStore, EngineHandle};
use ksni::blocking::TrayMethods;

#[derive(Clone, Copy, Debug)]
pub enum UiCommand {
    Show,
    Quit,
}

/// `MENU_ON_LEFT` is fixed when the tray is registered (ksni reads it as a
/// constant), so changing the setting re-registers the tray.
#[derive(Clone)]
struct AnimatrixTray<const MENU_ON_LEFT: bool> {
    config: Arc<Mutex<AppConfig>>,
    store: ConfigStore,
    engine: EngineHandle,
    ui: async_channel::Sender<UiCommand>,
}

impl<const MENU_ON_LEFT: bool> AnimatrixTray<MENU_ON_LEFT> {
    fn toggle(&self) {
        if let Ok(mut config) = self.config.lock() {
            config.enabled = !config.enabled;
        }
        self.save();
    }

    fn save(&self) {
        let snapshot = self.config.lock().ok().map(|config| config.clone());
        if let Some(config) = snapshot {
            if let Err(error) = self.store.save(&config) {
                eprintln!("animatrix: failed to save tray change: {error:#}");
            }
        }
        self.engine.refresh();
    }
}

impl<const MENU_ON_LEFT: bool> ksni::Tray for AnimatrixTray<MENU_ON_LEFT> {
    const MENU_ON_ACTIVATE: bool = MENU_ON_LEFT;

    fn id(&self) -> String {
        "net._512mb.Animatrix".into()
    }

    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let (enabled, inverted) = self.config.lock()
            .map(|config| (config.enabled, config.invert_tray_icon))
            .unwrap_or((false, false));
        tray_icons(enabled, inverted)
    }

    /// Left click toggles the light show, unless it opens the menu.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.toggle();
    }

    /// Middle click toggles the light show too, so it stays one click away
    /// when left click opens the menu.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.toggle();
    }

    fn title(&self) -> String {
        "Animatrix".into()
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;

        let snapshot = self.config.lock().map(|config| config.clone()).unwrap_or_default();
        let selected = snapshot
            .active_profile
            .as_ref()
            .and_then(|id| snapshot.profiles.iter().position(|profile| &profile.id == id))
            .unwrap_or(0);
        let options = snapshot
            .profiles
            .iter()
            .map(|profile| RadioItem {
                label: profile.name.clone(),
                ..Default::default()
            })
            .collect();

        vec![
            CheckmarkItem {
                label: "Light show enabled".into(),
                checked: snapshot.enabled,
                activate: Box::new(|this: &mut Self| this.toggle()),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Profiles".into(),
                submenu: vec![RadioGroup {
                    selected,
                    select: Box::new(|this: &mut Self, index| {
                        if let Ok(mut config) = this.config.lock() {
                            config.active_profile = config.profiles.get(index).map(|profile| profile.id.clone());
                        }
                        this.save();
                    }),
                    options,
                    ..Default::default()
                }
                .into()],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Animatrix".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.ui.send_blocking(UiCommand::Show);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.ui.send_blocking(UiCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

const DOCKLET_ON: &str = include_str!("../data/docklet-on.svg");
const DOCKLET_OFF: &str = include_str!("../data/docklet-off.svg");
/// Sizes offered to the tray host, which picks the closest one.
const ICON_SIZES: [u32; 5] = [16, 22, 24, 32, 48];

/// Rasterises the on/off docklet SVG, drawn in white instead of black when
/// `inverted`, into the ARGB pixmaps StatusNotifier hosts expect.
fn tray_icons(enabled: bool, inverted: bool) -> Vec<ksni::Icon> {
    let svg = if enabled { DOCKLET_ON } else { DOCKLET_OFF };
    let svg = if inverted { svg.replace("#000000", "#ffffff") } else { svg.to_owned() };
    let tree = match resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()) {
        Ok(tree) => tree,
        Err(error) => {
            eprintln!("animatrix: invalid tray icon: {error}");
            return Vec::new();
        }
    };
    ICON_SIZES.iter().filter_map(|&size| {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
        let scale = size as f32 / tree.size().width().max(tree.size().height());
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        // tiny-skia stores premultiplied RGBA; SNI wants straight ARGB.
        let data = pixmap.pixels().iter().flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.alpha(), color.red(), color.green(), color.blue()]
        }).collect();
        Some(ksni::Icon { width: size as i32, height: size as i32, data })
    }).collect()
}

/// What the tray shows; the tray is refreshed when this changes.
fn tray_state(config: &AppConfig) -> (bool, bool, Option<String>, Vec<String>) {
    (
        config.enabled,
        config.invert_tray_icon,
        config.active_profile.clone(),
        config.profiles.iter().map(|profile| profile.name.clone()).collect(),
    )
}

/// The registered tray, in whichever click layout it was built with.
enum Running {
    ToggleOnLeft(ksni::blocking::Handle<AnimatrixTray<false>>),
    MenuOnLeft(ksni::blocking::Handle<AnimatrixTray<true>>),
}

impl Running {
    fn spawn(tray: &AnimatrixTray<false>, menu_on_left: bool) -> Result<Self, ksni::Error> {
        Ok(if menu_on_left {
            let AnimatrixTray { config, store, engine, ui } = tray.clone();
            Self::MenuOnLeft(AnimatrixTray::<true> { config, store, engine, ui }.spawn()?)
        } else {
            Self::ToggleOnLeft(tray.clone().spawn()?)
        })
    }

    fn update(&self) {
        match self {
            Self::ToggleOnLeft(handle) => { handle.update(|_| {}); }
            Self::MenuOnLeft(handle) => { handle.update(|_| {}); }
        }
    }

    fn shutdown(self) {
        match self {
            Self::ToggleOnLeft(handle) => handle.shutdown().wait(),
            Self::MenuOnLeft(handle) => handle.shutdown().wait(),
        }
    }
}

pub fn start(
    config: Arc<Mutex<AppConfig>>,
    store: ConfigStore,
    engine: EngineHandle,
    ui: async_channel::Sender<UiCommand>,
) {
    std::thread::spawn(move || {
        let watched = Arc::clone(&config);
        let changes = engine.subscribe();
        let tray = AnimatrixTray { config, store, engine, ui };
        let menu_on_left = |config: &Arc<Mutex<AppConfig>>| {
            config.lock().map(|config| config.tray_menu_on_left_click).unwrap_or(false)
        };
        let mut layout = menu_on_left(&watched);
        let mut running = match Running::spawn(&tray, layout) {
            Ok(running) => running,
            Err(error) => {
                eprintln!("animatrix: tray unavailable: {error}");
                return;
            }
        };
        // Changes made in the window or by profile triggers do not go
        // through the tray; the engine reports every change, so wait for
        // those instead of polling.
        let mut shown = watched.lock().ok().map(|config| tray_state(&config));
        while changes.recv().is_ok() {
            if menu_on_left(&watched) != layout {
                layout = !layout;
                running.shutdown();
                running = match Running::spawn(&tray, layout) {
                    Ok(running) => running,
                    Err(error) => {
                        eprintln!("animatrix: tray unavailable: {error}");
                        return;
                    }
                };
                shown = watched.lock().ok().map(|config| tray_state(&config));
                continue;
            }
            let state = watched.lock().ok().map(|config| tray_state(&config));
            if state.is_some() && state != shown {
                running.update();
                shown = state;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docklet_icons_rasterise_in_every_variant() {
        for enabled in [false, true] {
            for inverted in [false, true] {
                let icons = tray_icons(enabled, inverted);
                assert_eq!(icons.len(), ICON_SIZES.len());
                for icon in &icons {
                    assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
                    // Opaque pixels are black, or white when inverted.
                    let opaque: Vec<_> = icon.data.chunks_exact(4).filter(|px| px[0] == 255).collect();
                    assert!(!opaque.is_empty());
                    let expected = if inverted { 255 } else { 0 };
                    assert!(opaque.iter().all(|px| px[1..] == [expected; 3]), "enabled={enabled} inverted={inverted}");
                }
            }
        }
        assert_ne!(tray_icons(true, false)[4].data, tray_icons(false, false)[4].data);
    }
}
