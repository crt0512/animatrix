#[cfg(feature = "tray")]
mod tray;
#[cfg(feature = "gui")]
mod remote;
#[cfg(feature = "gui")]
mod ui;

#[cfg(feature = "gui")]
fn main() -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex};

    use animatrix::{Autostart, ConfigStore, EngineHandle};

    // GTK rejects options it does not know, so take ours out first.
    let (minimized, args): (Vec<String>, Vec<String>) =
        std::env::args().partition(|arg| arg == "--minimized");
    if let Some(command) = remote::parse(args.get(1..).unwrap_or_default())? {
        return remote::run(command);
    }

    let store = ConfigStore::discover()?;
    let config = store.load()?;
    store.save(&config)?;
    if config.autostart {
        if let Err(error) = Autostart::discover().and_then(|autostart| autostart.ensure()) {
            eprintln!("animatrix: failed to install the login autostart entry: {error:#}");
        }
    }
    let shared = Arc::new(Mutex::new(config));
    let engine = EngineHandle::start(Arc::clone(&shared));
    ui::run(shared, store, engine, &args, !minimized.is_empty());
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!("animatrix was built without the 'gui' feature");
}
