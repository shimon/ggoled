#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hooks;
mod hooks_window;
mod os;

use chrono::{DateTime, Local, TimeDelta, Timelike};
use ggoled_draw::{DrawDevice, DrawEvent, LayerId, ShiftMode, TextRenderer, bitmap_from_memory};
use ggoled_lib::{Bitmap, Device};
use hooks::{Debouncer, HookKind, HooksConfig};
use os::{Media, OSFeatures, OSImpl};
use rfd::{MessageDialog, MessageLevel};
use sdl3_sys::everything as sdl;
use serde::{Deserialize, Serialize};
use std::{
    ffi::CStr,
    fmt::Debug,
    os::raw::c_void,
    path::PathBuf,
    process::Child,
    sync::{Arc, mpsc},
    thread::sleep,
    time::{Duration, Instant},
};

const NOTIF_DUR: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
enum ConfigTimeMode {
    Off,
    #[default]
    H24,
    H12,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
enum ConfigShiftMode {
    Off,
    Low,
    #[default]
    High,
}
impl ConfigShiftMode {
    fn to_api(self) -> ShiftMode {
        match self {
            ConfigShiftMode::Off => ShiftMode::Off,
            ConfigShiftMode::Low => ShiftMode::Low,
            ConfigShiftMode::High => ShiftMode::High,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq)]
enum ConfigQuitMode {
    #[default]
    Screensaver,
    StockUi,
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
enum StatusNotifyMode {
    Off,
    #[default]
    Notify,
    Always,
    WhenConnected,
    WhenDisconnected,
}

#[derive(Serialize, Deserialize, Default)]
struct ConfigFont {
    path: PathBuf,
    size: f32,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    font: Option<ConfigFont>,
    time_mode: ConfigTimeMode,
    show_media: bool,
    idle_timeout: bool,
    oled_shift: ConfigShiftMode,
    status_notify: StatusNotifyMode,
    quit_mode: ConfigQuitMode,
    hooks: HooksConfig,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            font: None,
            time_mode: ConfigTimeMode::default(),
            show_media: true,
            idle_timeout: true,
            oled_shift: ConfigShiftMode::default(),
            status_notify: StatusNotifyMode::default(),
            quit_mode: ConfigQuitMode::default(),
            hooks: HooksConfig::default(),
        }
    }
}
impl Config {
    fn path() -> PathBuf {
        directories::BaseDirs::new()
            .unwrap()
            .config_dir()
            .join("ggoled_app.toml")
    }
    pub fn save(&self) -> anyhow::Result<()> {
        let text = toml::to_string(self)?;
        std::fs::write(Self::path(), text)?;
        Ok(())
    }
    pub fn load() -> Config {
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return Config::default();
        };
        let Ok(conf) = toml::from_str(&text) else {
            return Config::default();
        };
        conf
    }
}

// unwrap an error and show a MessageDialog if it fails
pub fn dialog_unwrap<T, E: Debug>(res: Result<T, E>) -> T {
    match res {
        Ok(v) => v,
        Err(e) => {
            let str = format!("Error: {:?}", e);
            MessageDialog::new()
                .set_level(MessageLevel::Error)
                .set_title("ggoled")
                .set_description(&str)
                .show();
            panic!("dialog_unwrap: {}", str);
        }
    }
}

struct Icon {
    _pixels: Vec<u8>,
    surf: *mut sdl::SDL_Surface,
}
impl Icon {
    fn load(buf: &[u8]) -> Self {
        let pixels = image::load_from_memory(buf)
            .unwrap()
            .resize(32, 32, image::imageops::FilterType::Lanczos3)
            .to_rgba8()
            .into_vec();
        let surf = unsafe {
            sdl::SDL_CreateSurfaceFrom(
                32,
                32,
                sdl::SDL_PixelFormat::RGBA32,
                pixels.as_ptr() as *mut c_void,
                32 * 4,
            )
        };
        assert!(!surf.is_null());
        Self { _pixels: pixels, surf }
    }
}
impl Drop for Icon {
    fn drop(&mut self) {
        unsafe { sdl::SDL_DestroySurface(self.surf) };
    }
}

fn menu_check(menu: *mut sdl::SDL_TrayMenu, title: &'static CStr, checked: bool) -> *mut sdl::SDL_TrayEntry {
    let checked = if checked {
        sdl::SDL_TRAYENTRY_CHECKED
    } else {
        sdl::SDL_TrayEntryFlags(0)
    };
    unsafe { sdl::SDL_InsertTrayEntryAt(menu, -1, title.as_ptr(), sdl::SDL_TRAYENTRY_CHECKBOX | checked) }
}

extern "C" fn c_menu_callback(userdata: *mut c_void, _entry: *mut sdl::SDL_TrayEntry) {
    #[allow(clippy::borrowed_box)] // needs to be boxed
    let f: &Box<dyn Fn()> = unsafe { &*(userdata as *mut Box<dyn Fn()>) };
    f();
}

fn menu_callback(entry: *mut sdl::SDL_TrayEntry, f: impl Fn()) {
    let f: Box<Box<dyn Fn()>> = Box::new(Box::new(f));
    let f = Box::leak(f) as *mut Box<dyn Fn()> as *mut c_void;
    unsafe { sdl::SDL_SetTrayEntryCallback(entry, Some(c_menu_callback), f) };
}

#[derive(Clone, Copy)]
enum MenuEvent {
    ToggleCheck,
    SetTimeMode(ConfigTimeMode),
    SetShiftMode(ConfigShiftMode),
    SetStatusNotifyMode(StatusNotifyMode),
    SetQuitMode(ConfigQuitMode),
    OpenHooks,
    Quit,
}

fn bind_menu_event(entry: *mut sdl::SDL_TrayEntry, tx: &mpsc::Sender<MenuEvent>, event: MenuEvent) {
    let tx = tx.clone();
    menu_callback(entry, move || {
        let _ = tx.send(event);
    });
}

struct RadioMenu<T> {
    entries: Vec<(*mut sdl::SDL_TrayEntry, T)>,
}
impl<T: Copy + PartialEq> RadioMenu<T> {
    fn new(
        menu: *mut sdl::SDL_TrayMenu,
        title: &'static CStr,
        options: &[(&'static CStr, T)],
        initial: T,
        tx: &mpsc::Sender<MenuEvent>,
        into_event: fn(T) -> MenuEvent,
    ) -> Self {
        let submenu_entry = unsafe { sdl::SDL_InsertTrayEntryAt(menu, -1, title.as_ptr(), sdl::SDL_TRAYENTRY_SUBMENU) };
        let submenu = unsafe { sdl::SDL_CreateTraySubmenu(submenu_entry) };
        let entries: Vec<_> = options
            .iter()
            .map(|(label, value)| {
                let entry = menu_check(submenu, label, *value == initial);
                bind_menu_event(entry, tx, into_event(*value));
                (entry, *value)
            })
            .collect();
        Self { entries }
    }
    fn update_checked(&self, value: T) {
        for (entry, v) in &self.entries {
            unsafe { sdl::SDL_SetTrayEntryChecked(*entry, *v == value) };
        }
    }
}

// Hooks are edited by the hooks window process, so always read them fresh from disk
fn trigger_hook(kind: HookKind) {
    hooks::trigger(kind, &Config::load().hooks);
}

// Launch the hooks window as a separate process, unless it's already open
fn open_hooks_window(window: &mut Option<Child>) {
    if let Some(child) = window
        && matches!(child.try_wait(), Ok(None))
    {
        return;
    }
    *window = Some(dialog_unwrap(
        std::env::current_exe().and_then(|exe| std::process::Command::new(exe).arg("--hooks").spawn()),
    ));
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--hooks") {
        dialog_unwrap(hooks_window::run_window());
        return;
    }

    // Initial loading
    let mut config = Config::load();
    let mut os = OSImpl::new();

    // Create tray icon with menu
    unsafe { sdl::SDL_SetHint(sdl::SDL_HINT_VIDEO_ALLOW_SCREENSAVER, c"1".as_ptr()) };
    assert!(unsafe { sdl::SDL_Init(sdl::SDL_INIT_VIDEO) });
    let icon = Icon::load(include_bytes!("../assets/ggoled.png"));
    let icon_error = Icon::load(include_bytes!("../assets/ggoled_error.png"));
    let tray = unsafe { sdl::SDL_CreateTray(icon_error.surf, c"ggoled".as_ptr()) };
    assert!(!tray.is_null());
    let menu = unsafe { sdl::SDL_CreateTrayMenu(tray) };
    assert!(!menu.is_null());
    let (menu_tx, menu_rx) = mpsc::channel::<MenuEvent>();

    let version_label = format!("ggoled_app - v{}\0", env!("CARGO_PKG_VERSION"));
    unsafe {
        let tm_version = sdl::SDL_InsertTrayEntryAt(menu, -1, version_label.as_ptr().cast(), sdl::SDL_TRAYENTRY_BUTTON);
        sdl::SDL_SetTrayEntryEnabled(tm_version, false);
        sdl::SDL_InsertTrayEntryAt(menu, -1, std::ptr::null(), sdl::SDL_TRAYENTRY_BUTTON); // separator
    }

    let tm_time_radio = RadioMenu::new(
        menu,
        c"Time",
        &[
            (c"Off", ConfigTimeMode::Off),
            (c"24-hour", ConfigTimeMode::H24),
            (c"12-hour (AM/PM)", ConfigTimeMode::H12),
        ],
        config.time_mode,
        &menu_tx,
        MenuEvent::SetTimeMode,
    );

    let tm_media_check = menu_check(menu, c"Show playing media", config.show_media);
    if os.supports_media() {
        bind_menu_event(tm_media_check, &menu_tx, MenuEvent::ToggleCheck);
    } else {
        unsafe {
            config.show_media = false;
            sdl::SDL_SetTrayEntryChecked(tm_media_check, false);
            sdl::SDL_SetTrayEntryEnabled(tm_media_check, false);
        }
    }

    let tm_status_notify = RadioMenu::new(
        menu,
        c"Connection status icon",
        &[
            (c"Off", StatusNotifyMode::Off),
            (c"Notify", StatusNotifyMode::Notify),
            (c"Always (burn-in risk)", StatusNotifyMode::Always),
            (c"When connected (burn-in risk)", StatusNotifyMode::WhenConnected),
            (c"When disconnected (burn-in risk)", StatusNotifyMode::WhenDisconnected),
        ],
        config.status_notify,
        &menu_tx,
        MenuEvent::SetStatusNotifyMode,
    );

    let tm_idle_check = menu_check(menu, c"Screensaver when idle", config.idle_timeout);
    if os.supports_idle() {
        bind_menu_event(tm_idle_check, &menu_tx, MenuEvent::ToggleCheck);
    } else {
        unsafe {
            config.idle_timeout = false;
            sdl::SDL_SetTrayEntryChecked(tm_idle_check, false);
            sdl::SDL_SetTrayEntryEnabled(tm_idle_check, false);
        }
    }

    let tm_shift_radio = RadioMenu::new(
        menu,
        c"OLED screen shift",
        &[
            (c"Off", ConfigShiftMode::Off),
            (c"Low", ConfigShiftMode::Low),
            (c"High", ConfigShiftMode::High),
        ],
        config.oled_shift,
        &menu_tx,
        MenuEvent::SetShiftMode,
    );

    let tm_quit_mode_radio = RadioMenu::new(
        menu,
        c"On app quit",
        &[
            (c"Screensaver", ConfigQuitMode::Screensaver),
            (c"Stock UI", ConfigQuitMode::StockUi),
        ],
        config.quit_mode,
        &menu_tx,
        MenuEvent::SetQuitMode,
    );

    let tm_hooks = unsafe { sdl::SDL_InsertTrayEntryAt(menu, -1, c"Hooks…".as_ptr(), sdl::SDL_TRAYENTRY_BUTTON) };
    bind_menu_event(tm_hooks, &menu_tx, MenuEvent::OpenHooks);

    unsafe { sdl::SDL_InsertTrayEntryAt(menu, -1, std::ptr::null(), sdl::SDL_TRAYENTRY_BUTTON) }; // separator
    let tm_quit = unsafe { sdl::SDL_InsertTrayEntryAt(menu, -1, c"Quit".as_ptr(), sdl::SDL_TRAYENTRY_BUTTON) };
    bind_menu_event(tm_quit, &menu_tx, MenuEvent::Quit);

    // Load icons
    let icon_hs_connect =
        Arc::new(bitmap_from_memory(include_bytes!("../assets/headset_connected.png"), 0x80).unwrap());
    let icon_hs_disconnect =
        Arc::new(bitmap_from_memory(include_bytes!("../assets/headset_disconnected.png"), 0x80).unwrap());

    let notif_update = |dev: &mut DrawDevice,
                        layer: &mut Option<LayerId>,
                        mode: StatusNotifyMode,
                        connected: bool,
                        expiry: DateTime<Local>| {
        if let Some(id) = layer.take() {
            dev.remove_layer(id);
        }
        let show = match mode {
            StatusNotifyMode::Off => false,
            StatusNotifyMode::Notify => Local::now() < expiry,
            StatusNotifyMode::Always => true,
            StatusNotifyMode::WhenConnected => connected,
            StatusNotifyMode::WhenDisconnected => !connected,
        };
        if show {
            *layer = Some(
                dev.add_layer(ggoled_draw::DrawLayer::Image {
                    bitmap: (if connected {
                        &icon_hs_connect
                    } else {
                        &icon_hs_disconnect
                    })
                    .clone(),
                    x: 4,
                    y: 4,
                }),
            );
        }
    };

    // State
    let mut last_time = Local::now() - TimeDelta::seconds(1);
    let mut last_media: Option<Media> = None;
    let mut time_layers: Vec<LayerId> = vec![];
    let mut media_layers: Vec<LayerId> = vec![];
    let mut notif_layer: Option<LayerId> = None;
    let mut notif_expiry = Local::now();
    let mut is_connected = false;
    let mut headset_hooks = Debouncer::new(false);
    let mut base_hooks = Debouncer::new(false);
    let mut hooks_window: Option<Child> = None;

    // Wait for connect
    let mut last_connect = Instant::now() - Duration::from_secs(1);
    let dev = loop {
        let mut event = sdl::SDL_Event::default();
        while unsafe { sdl::SDL_PollEvent(&mut event) } {
            if sdl::SDL_EventType(unsafe { event.r#type }) == sdl::SDL_EVENT_QUIT {
                return;
            }
        }
        while let Ok(event) = menu_rx.try_recv() {
            match event {
                MenuEvent::Quit => return,
                MenuEvent::OpenHooks => open_hooks_window(&mut hooks_window),
                _ => {}
            }
        }
        if last_connect.elapsed() >= Duration::from_secs(1) {
            last_connect = Instant::now();
            if let Ok(d) = Device::connect() {
                break d;
            }
        }
        sleep(Duration::from_millis(10));
    };
    unsafe { sdl::SDL_SetTrayIcon(tray, icon.surf) };
    let mut base_connected = true;
    let mut dev = DrawDevice::new(dev, 30);
    if let Some(font) = &config.font {
        dev.texter = dialog_unwrap(TextRenderer::load_from_file(&font.path, font.size));
    }
    dev.probe();

    // Go!
    dev.set_shift_mode(config.oled_shift.to_api());
    dev.play();
    'main: loop {
        // Window event loop
        // TODO: handle system going to sleep
        let mut event = sdl::SDL_Event::default();
        while unsafe { sdl::SDL_PollEvent(&mut event) } {
            let event_type = sdl::SDL_EventType(unsafe { event.r#type });
            if event_type == sdl::SDL_EVENT_QUIT {
                break 'main;
            }
        }

        // Handle tray menu events
        let mut config_updated = false;
        while let Ok(event) = menu_rx.try_recv() {
            match event {
                MenuEvent::ToggleCheck => {
                    config_updated = true;
                    config.show_media = unsafe { sdl::SDL_GetTrayEntryChecked(tm_media_check) };
                    config.idle_timeout = unsafe { sdl::SDL_GetTrayEntryChecked(tm_idle_check) };
                }
                MenuEvent::SetTimeMode(mode) => {
                    config_updated = true;
                    config.time_mode = mode;
                    tm_time_radio.update_checked(mode);
                }
                MenuEvent::SetStatusNotifyMode(mode) => {
                    config_updated = true;
                    config.status_notify = mode;
                    tm_status_notify.update_checked(mode);
                    notif_update(&mut dev, &mut notif_layer, mode, is_connected, notif_expiry);
                }
                MenuEvent::SetShiftMode(mode) => {
                    config_updated = true;
                    config.oled_shift = mode;
                    tm_shift_radio.update_checked(mode);
                    dev.set_shift_mode(config.oled_shift.to_api());
                }
                MenuEvent::SetQuitMode(mode) => {
                    config_updated = true;
                    config.quit_mode = mode;
                    tm_quit_mode_radio.update_checked(mode);
                }
                MenuEvent::OpenHooks => open_hooks_window(&mut hooks_window),
                MenuEvent::Quit => break 'main,
            }
        }
        if config_updated {
            config.hooks = Config::load().hooks; // don't overwrite changes from the hooks window
            dialog_unwrap(config.save());
        }

        let mut force_redraw = config_updated;

        // Handle events
        while let Some(event) = dev.try_event() {
            #[cfg(debug_assertions)]
            println!("event: {:?}", event);
            match event {
                DrawEvent::DeviceDisconnected => {
                    unsafe { sdl::SDL_SetTrayIcon(tray, icon_error.surf) };
                    base_connected = false;
                    // The headset can't stay connected without its base station
                    is_connected = false;
                }
                DrawEvent::DeviceReconnected => {
                    unsafe { sdl::SDL_SetTrayIcon(tray, icon.surf) }
                    base_connected = true;
                    dev.probe(); // re-probe when base station reappears
                }
                #[allow(clippy::single_match)]
                DrawEvent::DeviceEvent(event) => match event {
                    ggoled_lib::DeviceEvent::HeadsetConnection { wireless, .. } => {
                        if wireless != is_connected {
                            is_connected = wireless;
                            notif_expiry = Local::now() + NOTIF_DUR;
                            notif_update(
                                &mut dev,
                                &mut notif_layer,
                                config.status_notify,
                                is_connected,
                                notif_expiry,
                            );
                            force_redraw = true;
                        }
                    }
                    _ => {}
                },
            }
        }

        // Run hooks once connection changes have settled
        let now = Instant::now();
        let hooks_delay = || Duration::from_millis(Config::load().hooks.debounce_ms);
        if let Some(connected) = base_hooks.update(base_connected, now, hooks_delay) {
            trigger_hook(if connected {
                HookKind::BaseStationConnect
            } else {
                HookKind::BaseStationDisconnect
            });
        }
        if let Some(connected) = headset_hooks.update(is_connected, now, hooks_delay) {
            trigger_hook(if connected {
                HookKind::HeadsetConnect
            } else {
                HookKind::HeadsetDisconnect
            });
        }

        // Update layers every second
        let time = Local::now();
        if time.second() != last_time.second() || force_redraw {
            last_time = time;

            // Remove expired notifications in Notify mode
            if config.status_notify == StatusNotifyMode::Notify
                && time >= notif_expiry
                && let Some(layer) = notif_layer.take()
            {
                dev.remove_layer(layer);
            }

            // Check if idle
            if config.idle_timeout && os.is_idle() {
                dev.clear_layers(); // clear screen when idle
                notif_layer = None;
                last_media = None; // reset media so we check again when not idle
            } else {
                // Update notifications
                if notif_layer.is_none() {
                    notif_update(
                        &mut dev,
                        &mut notif_layer,
                        config.status_notify,
                        is_connected,
                        notif_expiry,
                    );
                }

                // Fetch media once a second (before pausing screen)
                let media = if config.show_media { os.get_media() } else { None };

                dev.pause();

                // Time
                dev.remove_layers(&time_layers);
                if config.time_mode != ConfigTimeMode::Off {
                    let time_str = match config.time_mode {
                        ConfigTimeMode::Off => unreachable!(),
                        ConfigTimeMode::H24 => time.format("%H:%M:%S").to_string(),
                        ConfigTimeMode::H12 => time.format("%l:%M:%S %p").to_string(),
                    };
                    time_layers = dev.add_text(&time_str, None, if media.is_some() { Some(8) } else { None });
                }

                // Media
                if media != last_media {
                    dev.remove_layers(&media_layers);
                    if let Some(m) = &media {
                        media_layers = dev.add_text(
                            &format!("{}\n{}", m.title, m.artist),
                            None,
                            Some(8 + dev.font_line_height() as isize),
                        );
                    }
                    last_media = media;
                }

                dev.play();
            }
        }

        sleep(Duration::from_millis(10));
    }
    let dev = dev.stop();
    match config.quit_mode {
        ConfigQuitMode::Screensaver => dev.draw(&Bitmap::new(dev.width, dev.height, false), 0, 0),
        ConfigQuitMode::StockUi => dev.return_to_ui(),
    }
    .unwrap();
}
