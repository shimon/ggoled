use ggoled_lib::{Device, DeviceEvent};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    thread::sleep,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const POLL_TIMEOUT: Duration = Duration::from_millis(250);
const RECONNECT_PERIOD: Duration = Duration::from_secs(2);

#[derive(clap::Args)]
pub struct WatchArgs {
    #[arg(long, help = "Shell command to run when the headset connects wirelessly")]
    on_connect: Option<String>,

    #[arg(long, help = "Shell command to run when the headset disconnects")]
    on_disconnect: Option<String>,

    #[arg(
        long,
        help = "Write current state as JSON to this file on every change",
        long_help = "Write current state as JSON to this file on every change. \
The file is replaced atomically, so readers never see a partial write. Parent directories are created if needed."
    )]
    state_file: Option<PathBuf>,

    #[arg(
        long,
        help = "Milliseconds a new state must persist before it is reported",
        long_help = "Milliseconds a new state must persist before it is reported. \
Changes that revert within this time (e.g. a brief radio dropout) produce no output or hooks.",
        default_value = "1500"
    )]
    debounce: u64,

    #[arg(
        long,
        help = "Seconds between re-probing the device for its full state",
        long_help = "Seconds between re-probing the device for its full state. \
Changes are normally pushed by the device immediately; this catches any that were missed.",
        default_value = "30"
    )]
    probe_interval: u64,

    #[arg(long, help = "Don't run a hook for the initial state on startup")]
    no_initial_hook: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct WatchState {
    base_station: bool,
    wireless: Option<bool>,
    bluetooth: Option<bool>,
    bluetooth_on: Option<bool>,
    battery: Option<u8>,
    charging: Option<u8>,
}
impl WatchState {
    fn disconnected() -> Self {
        WatchState {
            wireless: Some(false),
            ..Default::default()
        }
    }

    /// Whether the headset connection state is known, i.e. reportable.
    fn is_known(&self) -> bool {
        self.wireless.is_some()
    }

    fn apply(&mut self, event: DeviceEvent) {
        match event {
            DeviceEvent::HeadsetConnection {
                wireless,
                bluetooth,
                bluetooth_on,
            } => {
                self.wireless = Some(wireless);
                self.bluetooth = Some(bluetooth);
                self.bluetooth_on = Some(bluetooth_on);
            }
            DeviceEvent::Battery { headset, charging } => {
                self.battery = Some(headset);
                self.charging = Some(charging);
            }
            DeviceEvent::Volume { .. } => {}
        }
    }

    fn to_json(&self, time: u64) -> String {
        fn opt<T: ToString>(v: Option<T>) -> String {
            v.map_or("null".into(), |v| v.to_string())
        }
        format!(
            r#"{{"time":{},"base_station":{},"wireless":{},"bluetooth":{},"bluetooth_on":{},"battery":{},"charging":{}}}"#,
            time,
            self.base_station,
            opt(self.wireless),
            opt(self.bluetooth),
            opt(self.bluetooth_on),
            opt(self.battery),
            opt(self.charging),
        )
    }

    fn env(&self) -> Vec<(&'static str, String)> {
        fn opt<T: ToString>(v: Option<T>) -> String {
            v.map_or(String::new(), |v| v.to_string())
        }
        vec![
            ("GGOLED_BASE_STATION", self.base_station.to_string()),
            ("GGOLED_WIRELESS", opt(self.wireless)),
            ("GGOLED_BLUETOOTH", opt(self.bluetooth)),
            ("GGOLED_BATTERY", opt(self.battery)),
            ("GGOLED_CHARGING", opt(self.charging)),
        ]
    }
}

#[derive(Debug, PartialEq)]
enum Hook {
    Connect,
    Disconnect,
}

struct Report {
    state: WatchState,
    hook: Option<Hook>,
}

/// Decides when an observed state has settled and should be reported, and which hook to run.
struct Reporter {
    debounce: Duration,
    initial_hook: bool,
    reported: Option<WatchState>,
    pending: Option<(WatchState, Instant)>,
}
impl Reporter {
    fn new(debounce: Duration, initial_hook: bool) -> Self {
        Reporter {
            debounce,
            initial_hook,
            reported: None,
            pending: None,
        }
    }

    fn update(&mut self, observed: &WatchState, now: Instant) -> Option<Report> {
        if !observed.is_known() || self.reported.as_ref() == Some(observed) {
            self.pending = None;
            return None;
        }
        // Report state once it has settled for the debounce period
        if self.pending.as_ref().is_none_or(|(s, _)| s != observed) {
            self.pending = Some((observed.clone(), now));
        }
        let (state, since) = self.pending.as_ref()?;
        if now.duration_since(*since) < self.debounce {
            return None;
        }
        let state = state.clone();
        self.pending = None;

        let prev_wireless = self.reported.as_ref().and_then(|s| s.wireless);
        let is_initial = self.reported.is_none();
        let hook = if state.wireless == prev_wireless || (is_initial && !self.initial_hook) {
            None
        } else if state.wireless == Some(true) {
            Some(Hook::Connect)
        } else {
            Some(Hook::Disconnect)
        };
        self.reported = Some(state.clone());
        Some(Report { state, hook })
    }
}

fn write_state_file(path: &Path, json: &str) {
    // Write to a temp file and rename so readers never see a partial file
    let tmp = path.with_extension("tmp");
    let res = fs::write(&tmp, format!("{json}\n")).and_then(|_| fs::rename(&tmp, path));
    if let Err(err) = res {
        eprintln!("Failed to write state file {}: {err}", path.display());
    }
}

fn run_hook(cmd: &str, state: &WatchState) {
    #[cfg(windows)]
    let mut command = Command::new("cmd");
    #[cfg(windows)]
    command.arg("/C").arg(cmd);
    #[cfg(not(windows))]
    let mut command = Command::new("sh");
    #[cfg(not(windows))]
    command.arg("-c").arg(cmd);

    command.envs(state.env());
    match command.spawn() {
        // Reap in the background so a slow hook doesn't block event handling
        Ok(mut child) => _ = std::thread::spawn(move || child.wait()),
        Err(err) => eprintln!("Failed to run hook `{cmd}`: {err}"),
    }
}

pub fn watch(args: WatchArgs) {
    if let Some(path) = &args.state_file
        && let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        _ = fs::create_dir_all(dir);
    }
    let probe_interval = Duration::from_secs(args.probe_interval.max(1));
    let mut reporter = Reporter::new(Duration::from_millis(args.debounce), !args.no_initial_hook);

    let mut dev: Option<Device> = None;
    let mut last_connect_attempt: Option<Instant> = None;
    let mut last_probe = Instant::now();
    let mut observed = WatchState::default();

    loop {
        // Connect to the base station, or update state from it
        match &dev {
            None => {
                if last_connect_attempt.is_none_or(|t| t.elapsed() >= RECONNECT_PERIOD) {
                    last_connect_attempt = Some(Instant::now());
                    match Device::connect() {
                        Ok(d) => {
                            observed = WatchState {
                                base_station: true,
                                ..Default::default()
                            };
                            if d.probe().is_ok() {
                                last_probe = Instant::now();
                                dev = Some(d);
                            }
                        }
                        Err(_) => observed = WatchState::disconnected(),
                    }
                }
                if dev.is_none() {
                    sleep(POLL_TIMEOUT);
                }
            }
            Some(d) => {
                let mut ok = match d.poll_event_timeout(POLL_TIMEOUT) {
                    Ok(events) => {
                        events.into_iter().for_each(|e| observed.apply(e));
                        true
                    }
                    Err(_) => false,
                };
                if ok && last_probe.elapsed() >= probe_interval {
                    last_probe = Instant::now();
                    ok = d.probe().is_ok();
                }
                if !ok {
                    // Base station went away; the headset can't be connected to it
                    dev = None;
                    last_connect_attempt = Some(Instant::now());
                    observed = WatchState::disconnected();
                }
            }
        }

        let Some(report) = reporter.update(&observed, Instant::now()) else {
            continue;
        };
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let json = report.state.to_json(time);
        println!("{json}");
        _ = std::io::stdout().flush();
        if let Some(path) = &args.state_file {
            write_state_file(path, &json);
        }
        let hook = match report.hook {
            Some(Hook::Connect) => &args.on_connect,
            Some(Hook::Disconnect) => &args.on_disconnect,
            None => &None,
        };
        if let Some(cmd) = hook {
            run_hook(cmd, &report.state);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(1500);

    fn headset(wireless: bool, battery: u8) -> WatchState {
        WatchState {
            base_station: true,
            wireless: Some(wireless),
            bluetooth: Some(false),
            bluetooth_on: Some(true),
            battery: Some(battery),
            charging: Some(0),
        }
    }

    #[test]
    fn reports_after_debounce() {
        let t = Instant::now();
        let mut r = Reporter::new(DEBOUNCE, true);
        let on = headset(true, 6);
        assert!(r.update(&on, t).is_none());
        assert!(r.update(&on, t + Duration::from_millis(1000)).is_none());
        let report = r.update(&on, t + DEBOUNCE).unwrap();
        assert_eq!(report.state, on);
        assert_eq!(report.hook, Some(Hook::Connect));
        // Unchanged state is not reported again
        assert!(r.update(&on, t + DEBOUNCE * 2).is_none());
    }

    #[test]
    fn ignores_brief_flaps() {
        let t = Instant::now();
        let mut r = Reporter::new(DEBOUNCE, true);
        let (on, off) = (headset(true, 6), headset(false, 6));
        r.update(&on, t);
        r.update(&on, t + DEBOUNCE).unwrap();
        // Disconnect shorter than the debounce period
        assert!(r.update(&off, t + Duration::from_secs(5)).is_none());
        assert!(r.update(&on, t + Duration::from_secs(6)).is_none());
        assert!(r.update(&on, t + Duration::from_secs(10)).is_none());
    }

    #[test]
    fn waits_for_known_state() {
        let t = Instant::now();
        let mut r = Reporter::new(Duration::ZERO, true);
        let probing = WatchState {
            base_station: true,
            ..Default::default()
        };
        assert!(r.update(&probing, t).is_none());
    }

    #[test]
    fn initial_hook_optional() {
        let t = Instant::now();
        let mut r = Reporter::new(Duration::ZERO, false);
        let report = r.update(&headset(false, 0), t).unwrap();
        assert_eq!(report.hook, None);
        // Subsequent changes still run hooks
        let report = r.update(&headset(true, 8), t).unwrap();
        assert_eq!(report.hook, Some(Hook::Connect));
    }

    #[test]
    fn battery_change_reports_without_hook() {
        let t = Instant::now();
        let mut r = Reporter::new(Duration::ZERO, true);
        r.update(&headset(true, 8), t).unwrap();
        let report = r.update(&headset(true, 7), t).unwrap();
        assert_eq!(report.state.battery, Some(7));
        assert_eq!(report.hook, None);
    }

    #[test]
    fn base_station_loss_disconnects() {
        let t = Instant::now();
        let mut r = Reporter::new(Duration::ZERO, true);
        r.update(&headset(true, 8), t).unwrap();
        let report = r.update(&WatchState::disconnected(), t).unwrap();
        assert_eq!(report.hook, Some(Hook::Disconnect));
    }

    #[test]
    fn json() {
        assert_eq!(
            headset(true, 6).to_json(42),
            r#"{"time":42,"base_station":true,"wireless":true,"bluetooth":false,"bluetooth_on":true,"battery":6,"charging":0}"#
        );
        assert_eq!(
            WatchState::disconnected().to_json(42),
            r#"{"time":42,"base_station":false,"wireless":false,"bluetooth":null,"bluetooth_on":null,"battery":null,"charging":null}"#
        );
    }
}
