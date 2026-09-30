use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::matrix;
use crate::model::{DevicePolicy, MatrixModel};

pub trait MatrixControl: Send + Sync + 'static {
    fn set_enabled(&self, enabled: bool) -> Result<()>;
    fn apply_policy(&self, policy: &DevicePolicy) -> Result<()>;
    /// Shows one frame, already mapped to LED order (see [`matrix`]).
    fn write_leds(&self, leds: Vec<u8>, model: MatrixModel) -> Result<()>;
}

/// Talks to asusd: frames go straight over D-Bus, settings through the
/// asusctl CLI.
#[derive(Debug)]
pub struct Asusctl {
    program: OsString,
    bus: Mutex<Option<zbus::blocking::Connection>>,
}

impl Default for Asusctl {
    fn default() -> Self {
        Self { program: OsString::from("asusctl"), bus: Mutex::new(None) }
    }
}

impl Asusctl {
    fn run<I, S>(&self, args: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new(&self.program)
            .args(args)
            .output()
            .with_context(|| format!("failed to launch {}", self.program.to_string_lossy()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!("asusctl failed ({}): {}", output.status, stderr);
        }
        Ok(())
    }

    /// Builds the asusctl calls needed to reach `policy`. When the current
    /// builtin state is known, redundant builtin/powersave writes are skipped:
    /// asusd's BuiltinsEnabled=false path makes the GA402 firmware STALL
    /// ("usb Pipe error"), and `set-builtins` always re-enables powersave.
    pub fn policy_commands(policy: &DevicePolicy, current: Option<&BuiltinState>) -> Vec<Vec<String>> {
        let mut commands = vec![vec![
            "anime".into(),
            "--off-when-unplugged".into(), policy.off_when_unplugged.to_string(),
            "--off-when-suspended".into(), policy.off_when_suspended.to_string(),
            // With a delay or "unless plugged in" the engine blanks the display itself.
            "--off-when-lid-closed".into(),
            (policy.off_when_lid_closed && !policy.engine_handles_lid()).to_string(),
            "--brightness".into(), policy.brightness.clone(),
        ]];
        let animations = [
            &policy.boot_animation, &policy.awake_animation,
            &policy.sleep_animation, &policy.shutdown_animation,
        ];
        let set_builtins = current.is_none_or(|state| state.animations.iter().ne(animations));
        if set_builtins {
            commands.push(vec![
                "anime".into(), "set-builtins".into(),
                "--boot".into(), policy.boot_animation.clone(),
                "--awake".into(), policy.awake_animation.clone(),
                "--sleep".into(), policy.sleep_animation.clone(),
                "--shutdown".into(), policy.shutdown_animation.clone(),
                "--set".into(), "true".into(),
            ]);
        }
        if set_builtins || current.is_some_and(|state| state.enabled != policy.powersave_animation) {
            commands.push(vec![
                "anime".into(),
                "--enable-powersave-anim".into(), policy.powersave_animation.to_string(),
            ]);
        }
        commands
    }

    fn builtin_state(&self) -> Option<BuiltinState> {
        let enabled = busctl_property("BuiltinsEnabled")?;
        let animations = busctl_property("BuiltinAnimations")?;
        let animations: Vec<String> = animations.split('"').skip(1).step_by(2).map(str::to_owned).collect();
        Some(BuiltinState {
            enabled: enabled.trim() == "b true",
            animations: animations.try_into().ok()?,
        })
    }

    pub fn display_args(enabled: bool) -> Vec<String> {
        vec!["anime".into(), "--enable-display".into(), enabled.to_string()]
    }

    /// Calls asusd's `Write` with a ready LED buffer, reconnecting once if
    /// the cached system bus connection went stale (e.g. asusd restarted).
    fn write_buffer(&self, data: Vec<u8>, anime_type: &str) -> Result<()> {
        let mut bus = self.bus.lock().map_err(|_| anyhow::anyhow!("D-Bus connection lock poisoned"))?;
        let mut last_error = None;
        for _ in 0..2 {
            let connection = match bus.as_ref() {
                Some(connection) => connection.clone(),
                None => {
                    let connection = zbus::blocking::Connection::system()
                        .context("failed to connect to the system D-Bus")?;
                    *bus = Some(connection.clone());
                    connection
                }
            };
            match connection.call_method(
                Some("xyz.ljones.Asusd"),
                "/xyz/ljones/aura/anime",
                Some("xyz.ljones.Anime"),
                "Write",
                &((data.as_slice(), anime_type),),
            ) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    *bus = None;
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.map(anyhow::Error::from).unwrap_or_else(|| anyhow::anyhow!("unreachable")))
            .context("asusd rejected the frame")
    }
}

impl MatrixControl for Asusctl {
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        self.run(Self::display_args(enabled))
    }

    fn apply_policy(&self, policy: &DevicePolicy) -> Result<()> {
        for command in Self::policy_commands(policy, self.builtin_state().as_ref()) {
            self.run(command)?;
        }
        Ok(())
    }

    fn write_leds(&self, leds: Vec<u8>, model: MatrixModel) -> Result<()> {
        self.write_buffer(leds, matrix::anime_type(model))
    }
}

/// Builtin animation state as currently reported by asusd.
#[derive(Clone, Debug, PartialEq)]
pub struct BuiltinState {
    pub enabled: bool,
    pub animations: [String; 4],
}

fn busctl_property(name: &str) -> Option<String> {
    let output = Command::new("busctl")
        .args(["--system", "get-property", "xyz.ljones.Asusd", "/xyz/ljones/aura/anime", "xyz.ljones.Anime", name])
        .output()
        .ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_uses_current_asusctl_long_option() {
        assert_eq!(Asusctl::display_args(false), ["anime", "--enable-display", "false"]);
    }

    #[test]
    fn policy_includes_all_lifecycle_states() {
        let commands = Asusctl::policy_commands(&DevicePolicy::default(), None);
        let builtins = commands[1].join(" ");
        for option in ["--boot", "--awake", "--sleep", "--shutdown", "--set"] {
            assert!(builtins.contains(option));
        }
        assert_eq!(commands[2][1], "--enable-powersave-anim");
    }

    #[test]
    fn policy_skips_builtins_already_applied() {
        let policy = DevicePolicy::default();
        let current = BuiltinState {
            enabled: policy.powersave_animation,
            animations: [
                policy.boot_animation.clone(), policy.awake_animation.clone(),
                policy.sleep_animation.clone(), policy.shutdown_animation.clone(),
            ],
        };
        assert_eq!(Asusctl::policy_commands(&policy, Some(&current)).len(), 1);
    }

    #[test]
    fn lid_delay_hands_lid_handling_to_the_engine() {
        let mut policy = DevicePolicy::default();
        let lid_flag = |policy: &DevicePolicy| {
            let flags = &Asusctl::policy_commands(policy, None)[0];
            let at = flags.iter().position(|arg| arg == "--off-when-lid-closed").unwrap();
            flags[at + 1].clone()
        };
        assert_eq!(lid_flag(&policy), "true");
        policy.lid_close_delay_secs = 30;
        assert_eq!(lid_flag(&policy), "false");
        policy.lid_close_delay_secs = 0;
        policy.lid_stay_on_when_plugged = true;
        assert_eq!(lid_flag(&policy), "false");
    }
}
