use serde::{Deserialize, Serialize};
use std::process::{Command, Stdio};

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

#[derive(Serialize, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct HooksConfig {
    pub headset_connect: HookConfig,
    pub headset_disconnect: HookConfig,
    pub base_station_connect: HookConfig,
    pub base_station_disconnect: HookConfig,
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn success_output() {
        let result = execute(HookKind::HeadsetConnect, "echo out; echo err >&2");
        assert_eq!(result, Ok("out\nerr".to_string()));
    }

    #[test]
    fn failure_status() {
        let result = execute(HookKind::HeadsetConnect, "echo oops >&2; exit 3");
        assert_eq!(result, Err("Exited with status 3:\noops".to_string()));
        let result = execute(HookKind::HeadsetConnect, "exit 1");
        assert_eq!(result, Err("Exited with status 1".to_string()));
    }

    #[test]
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
    }
}
