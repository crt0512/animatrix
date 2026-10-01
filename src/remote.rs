//! Commands for scripts: `animatrix --profile NAME` and friends hand the
//! request to the running instance over D-Bus (as GApplication actions) and
//! exit. The running instance does nothing for this until a call arrives.

use anyhow::{Context, Result, bail};
use animatrix::{AppConfig, ConfigStore};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;

pub const APP_ID: &str = "net._512mb.Animatrix";

pub enum Command {
    Profile(String),
    LightShow(LightShow),
    ListProfiles,
    Help,
}

#[derive(Clone, Copy)]
pub enum LightShow {
    On,
    Off,
    Toggle,
}

const USAGE: &str = "\
Usage: animatrix [--minimized]
       animatrix --profile NAME           switch the running instance to a profile (name or id)
       animatrix --light-show on|off|toggle
       animatrix --list-profiles          print profiles, the active one marked with *";

/// Finds a script command in `args` (without the program name); `None`
/// starts the app as usual.
pub fn parse(args: &[String]) -> Result<Option<Command>> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        let mut value = || inline.clone().or_else(|| args.next().cloned()).with_context(|| format!("{flag} needs a value\n{USAGE}"));
        return Ok(Some(match flag {
            "--profile" => Command::Profile(value()?),
            "--light-show" => Command::LightShow(match value()?.as_str() {
                "on" => LightShow::On,
                "off" => LightShow::Off,
                "toggle" => LightShow::Toggle,
                other => bail!("--light-show takes on, off or toggle, not {other:?}"),
            }),
            "--list-profiles" => Command::ListProfiles,
            "-h" | "--help" => Command::Help,
            _ => continue,
        }));
    }
    Ok(None)
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Help => println!("{USAGE}"),
        Command::ListProfiles => {
            let config = ConfigStore::discover()?.load()?;
            for profile in &config.profiles {
                let marker = if config.active_profile.as_ref() == Some(&profile.id) { '*' } else { ' ' };
                println!("{marker} {}\t{}", profile.name, profile.id);
            }
        }
        Command::Profile(query) => {
            // Checked here too, so a typo fails the script instead of only
            // showing up in the running instance's log.
            let config = ConfigStore::discover()?.load()?;
            let id = find_profile(&config, &query).map_err(anyhow::Error::msg)?;
            send("set-profile", Some(&id.to_variant()))?;
        }
        Command::LightShow(LightShow::Toggle) => send("toggle-light-show", None)?,
        Command::LightShow(state) => send("set-light-show", Some(&matches!(state, LightShow::On).to_variant()))?,
    }
    Ok(())
}

/// Activates an action on the running instance.
fn send(action: &str, parameter: Option<&glib::Variant>) -> Result<()> {
    let app = gio::Application::new(Some(APP_ID), gio::ApplicationFlags::empty());
    app.register(gio::Cancellable::NONE).context("could not reach the session bus")?;
    if !app.is_remote() {
        bail!("Animatrix is not running; start it first, e.g. `systemctl --user start animatrix`");
    }
    app.activate_action(action, parameter);
    // The call is queued; make sure it leaves before this process exits.
    if let Some(connection) = app.dbus_connection() {
        connection.flush_sync(gio::Cancellable::NONE)?;
    }
    Ok(())
}

/// A profile by id, by exact name, or by name ignoring case if that is
/// unambiguous.
pub fn find_profile(config: &AppConfig, query: &str) -> Result<String, String> {
    let profiles = &config.profiles;
    if let Some(profile) = profiles.iter().find(|profile| profile.id == query || profile.name == query) {
        return Ok(profile.id.clone());
    }
    let matches: Vec<_> = profiles.iter().filter(|profile| profile.name.eq_ignore_ascii_case(query)).collect();
    match matches.as_slice() {
        [profile] => Ok(profile.id.clone()),
        [] => Err(format!("no profile named {query:?}; see `animatrix --list-profiles`")),
        _ => Err(format!("several profiles are named {query:?}; use its id from `animatrix --list-profiles`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use animatrix::DisplayProfile;

    #[test]
    fn profiles_are_found_by_id_name_or_unambiguous_case() {
        let mut config = AppConfig::default();
        config.profiles = vec![DisplayProfile::new("Night", Vec::new()), DisplayProfile::new("Work", Vec::new())];
        let (night, work) = (config.profiles[0].id.clone(), config.profiles[1].id.clone());
        assert_eq!(find_profile(&config, &work), Ok(work.clone()));
        assert_eq!(find_profile(&config, "Night"), Ok(night.clone()));
        assert_eq!(find_profile(&config, "night"), Ok(night));
        assert!(find_profile(&config, "Gaming").is_err());
        config.profiles.push(DisplayProfile::new("WORK", Vec::new()));
        assert_eq!(find_profile(&config, "Work"), Ok(work));
        assert!(find_profile(&config, "work").is_err());
    }

    #[test]
    fn script_flags_are_parsed() {
        let parse = |args: &[&str]| parse(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>());
        assert!(matches!(parse(&[]), Ok(None)));
        assert!(matches!(parse(&["--profile", "Night"]), Ok(Some(Command::Profile(name))) if name == "Night"));
        assert!(matches!(parse(&["--profile=Night Mode"]), Ok(Some(Command::Profile(name))) if name == "Night Mode"));
        assert!(matches!(parse(&["--light-show", "toggle"]), Ok(Some(Command::LightShow(LightShow::Toggle)))));
        assert!(parse(&["--light-show", "dim"]).is_err());
        assert!(parse(&["--profile"]).is_err());
    }
}
