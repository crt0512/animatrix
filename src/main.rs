#[cfg(feature = "tray")]
mod tray;
#[cfg(feature = "gui")]
mod ui;

#[cfg(feature = "gui")]
fn main() -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex};

    use animatrix::{ConfigStore, EngineHandle};

    let store = ConfigStore::discover()?;
    let config = store.load()?;
    store.save(&config)?;
    let shared = Arc::new(Mutex::new(config));
    let engine = EngineHandle::start(Arc::clone(&shared));
    ui::run(shared, store, engine);
    Ok(())
}

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!("animatrix was built without the 'gui' feature");
}
