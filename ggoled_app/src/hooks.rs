use serde::{Deserialize, Serialize};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const NOTIF_BODY_MAX: usize = 300;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct HookConfig {
    pub enabled: bool,
    pub notify: bool,
    pub command: String,
}
impl Default for HookConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            notify: false,
            command: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default)]
pub struct HooksConfig {
    pub headset_connect: HookConfig,
    pub headset_disconnect: HookConfig,
    pub base_station_connect: HookConfig,
    pub base_station_disconnect: HookConfig,
    /// How long a connection change must persist before hooks run.
    pub debounce_ms: u64,
}
impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            headset_connect: HookConfig::default(),
            headset_disconnect: HookConfig::default(),
            base_station_connect: HookConfig::default(),
            base_station_disconnect: HookConfig::default(),
            debounce_ms: 1500,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookKind {
    HeadsetConnect,
    HeadsetDisconnect,
    BaseStationConnect,
    BaseStationDisconnect,
}
impl HookKind {
    pub const ALL: [HookKind; 4] = [
        HookKind::HeadsetConnect,
        HookKind::HeadsetDisconnect,
        HookKind::BaseStationConnect,
        HookKind::BaseStationDisconnect,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HookKind::HeadsetConnect => "Headset connected",
            HookKind::HeadsetDisconnect => "Headset disconnected",
            HookKind::BaseStationConnect => "Base station connected",
            HookKind::BaseStationDisconnect => "Base station disconnected",
        }
    }

    /// Passed to hook commands as `GGOLED_EVENT`.
    pub fn event_name(self) -> &'static str {
        match self {
            HookKind::HeadsetConnect => "headset_connect",
            HookKind::HeadsetDisconnect => "headset_disconnect",
            HookKind::BaseStationConnect => "base_station_connect",
            HookKind::BaseStationDisconnect => "base_station_disconnect",
        }
    }

    pub fn get(self, hooks: &HooksConfig) -> &HookConfig {
        match self {
            HookKind::HeadsetConnect => &hooks.headset_connect,
            HookKind::HeadsetDisconnect => &hooks.headset_disconnect,
            HookKind::BaseStationConnect => &hooks.base_station_connect,
            HookKind::BaseStationDisconnect => &hooks.base_station_disconnect,
        }
    }

    pub fn get_mut(self, hooks: &mut HooksConfig) -> &mut HookConfig {
        match self {
            HookKind::HeadsetConnect => &mut hooks.headset_connect,
            HookKind::HeadsetDisconnect => &mut hooks.headset_disconnect,
            HookKind::BaseStationConnect => &mut hooks.base_station_connect,
            HookKind::BaseStationDisconnect => &mut hooks.base_station_disconnect,
        }
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let mut cmd = Command::new("cmd");
        cmd.arg("/C").arg(command).creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

/// Run a hook command to completion. Returns its output on success, or an error description.
fn execute(kind: HookKind, command: &str) -> Result<String, String> {
    let output = shell_command(command)
        .env("GGOLED_EVENT", kind.event_name())
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("Failed to start: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{}\n{}", stdout.trim(), stderr.trim()).trim().to_string();
    if output.status.success() {
        Ok(text)
    } else {
        let status = match output.status.code() {
            Some(code) => format!("Exited with status {code}"),
            None => "Terminated by signal".to_string(),
        };
        Err(if text.is_empty() {
            status
        } else {
            format!("{status}:\n{text}")
        })
    }
}

fn notify(kind: HookKind, result: &Result<String, String>) {
    let (summary, body) = match result {
        Ok(output) => (format!("{} hook ran", kind.label()), output.as_str()),
        Err(err) => (format!("{} hook failed", kind.label()), err.as_str()),
    };
    let body: String = if body.chars().count() > NOTIF_BODY_MAX {
        body.chars().take(NOTIF_BODY_MAX).chain("…".chars()).collect()
    } else {
        body.to_string()
    };
    _ = notify_rust::Notification::new()
        .appname("ggoled")
        .summary(&summary)
        .body(&body)
        .show();
}

/// Run a hook, showing a notification if it fails or if `hook.notify` is set. Blocks until done.
pub fn run(kind: HookKind, hook: &HookConfig) -> Result<String, String> {
    let result = execute(kind, &hook.command);
    if result.is_err() || hook.notify {
        notify(kind, &result);
    }
    result
}

/// Run a hook in the background if it is enabled and has a command.
pub fn trigger(kind: HookKind, hooks: &HooksConfig) {
    let hook = kind.get(hooks);
    if hook.enabled && !hook.command.trim().is_empty() {
        let hook = hook.clone();
        std::thread::spawn(move || run(kind, &hook));
    }
}

/// Reports a connection state once it has stayed changed for a delay, so brief dropouts don't run hooks.
pub struct Debouncer {
    reported: bool,
    pending: Option<(Instant, Duration)>,
}
impl Debouncer {
    pub fn new(initial: bool) -> Self {
        Self {
            reported: initial,
            pending: None,
        }
    }

    /// Returns the new state once it has settled. `delay` is only called when a change starts.
    pub fn update(&mut self, value: bool, now: Instant, delay: impl FnOnce() -> Duration) -> Option<bool> {
        if value == self.reported {
            self.pending = None;
            return None;
        }
        let (since, delay) = *self.pending.get_or_insert_with(|| (now, delay()));
        if now.duration_since(since) < delay {
            return None;
        }
        self.reported = value;
        self.pending = None;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: Duration = Duration::from_millis(1500);

    #[test]
    fn debounce_reports_after_delay() {
        let t = Instant::now();
        let mut d = Debouncer::new(false);
        assert_eq!(d.update(true, t, || DELAY), None);
        assert_eq!(d.update(true, t + Duration::from_millis(1000), || DELAY), None);
        assert_eq!(d.update(true, t + DELAY, || DELAY), Some(true));
        assert_eq!(d.update(true, t + DELAY * 2, || DELAY), None);
        assert_eq!(d.update(false, t + DELAY * 2, || Duration::ZERO), Some(false));
    }

    #[test]
    fn debounce_ignores_flaps() {
        let t = Instant::now();
        let mut d = Debouncer::new(true);
        assert_eq!(d.update(false, t, || DELAY), None);
        assert_eq!(d.update(true, t + Duration::from_secs(1), || DELAY), None);
        // A new change restarts the delay
        assert_eq!(d.update(false, t + Duration::from_secs(2), || DELAY), None);
        assert_eq!(d.update(false, t + Duration::from_secs(3), || DELAY), None);
        assert_eq!(d.update(false, t + Duration::from_secs(4), || DELAY), Some(false));
    }

    #[test]
    #[cfg(unix)]
    fn success_output() {
        let result = execute(HookKind::HeadsetConnect, "echo out; echo err >&2");
        assert_eq!(result, Ok("out\nerr".to_string()));
    }

    #[test]
    #[cfg(unix)]
    fn failure_status() {
        let result = execute(HookKind::HeadsetConnect, "echo oops >&2; exit 3");
        assert_eq!(result, Err("Exited with status 3:\noops".to_string()));
        let result = execute(HookKind::HeadsetConnect, "exit 1");
        assert_eq!(result, Err("Exited with status 1".to_string()));
    }

    #[test]
    #[cfg(unix)]
    fn event_env() {
        let result = execute(HookKind::BaseStationDisconnect, "echo $GGOLED_EVENT");
        assert_eq!(result, Ok("base_station_disconnect".to_string()));
    }

    #[test]
    fn config_defaults() {
        let hooks: HooksConfig = toml::from_str("[headset_connect]\ncommand = \"true\"").unwrap();
        assert!(hooks.headset_connect.enabled);
        assert!(!hooks.headset_connect.notify);
        assert_eq!(hooks.headset_connect.command, "true");
        assert!(hooks.base_station_connect == HookConfig::default());
        assert_eq!(hooks.debounce_ms, 1500);
    }
}
