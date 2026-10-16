use crate::{
    Config,
    hooks::{self, HookKind, HooksConfig},
};
use eframe::egui;
use std::sync::{Arc, Mutex};

enum TestState {
    Running,
    Done(Result<String, String>),
}

type TestSlot = Arc<Mutex<Option<TestState>>>;

struct HooksApp {
    hooks: HooksConfig,
    tests: Vec<TestSlot>,
    save_error: Option<String>,
}
impl HooksApp {
    fn new() -> Self {
        Self {
            hooks: Config::load().hooks,
            tests: HookKind::ALL.iter().map(|_| TestSlot::default()).collect(),
            save_error: None,
        }
    }

    fn save(&mut self) {
        // Only replace hooks, keeping settings the tray app may have changed meanwhile
        let mut config = Config::load();
        config.hooks = self.hooks.clone();
        self.save_error = config.save().err().map(|err| format!("Failed to save config: {err}"));
    }
}

impl eframe::App for HooksApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut changed = false;
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.label(
                    "Shell commands to run when devices connect or disconnect. \
                     The event name is passed in the GGOLED_EVENT environment variable.",
                );
                if let Some(err) = &self.save_error {
                    ui.colored_label(ui.visuals().error_fg_color, err);
                }
                for (kind, test) in HookKind::ALL.into_iter().zip(&self.tests) {
                    ui.separator();
                    let hook = kind.get_mut(&mut self.hooks);
                    let running = matches!(*test.lock().unwrap(), Some(TestState::Running));
                    ui.horizontal(|ui| {
                        ui.strong(kind.label());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let can_test = !running && !hook.command.trim().is_empty();
                            if ui
                                .add_enabled(can_test, egui::Button::new("Test"))
                                .on_hover_text("Run the command now, even if disabled")
                                .clicked()
                            {
                                *test.lock().unwrap() = Some(TestState::Running);
                                let (hook, test, ctx) = (hook.clone(), test.clone(), ui.ctx().clone());
                                std::thread::spawn(move || {
                                    let result = hooks::run(kind, &hook);
                                    *test.lock().unwrap() = Some(TestState::Done(result));
                                    ctx.request_repaint();
                                });
                            }
                            changed |= ui
                                .checkbox(&mut hook.notify, "Notify")
                                .on_hover_text("Show a notification every time this runs (failures always notify)")
                                .changed();
                            changed |= ui.checkbox(&mut hook.enabled, "Enabled").changed();
                        });
                    });
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut hook.command)
                                .hint_text("Shell command")
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY),
                        )
                        .changed();
                    match &*test.lock().unwrap() {
                        None => {}
                        Some(TestState::Running) => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Running…");
                            });
                        }
                        Some(TestState::Done(Ok(output))) => {
                            ui.colored_label(egui::Color32::from_rgb(0x40, 0xb0, 0x40), "✔ Succeeded");
                            if !output.is_empty() {
                                ui.monospace(output);
                            }
                        }
                        Some(TestState::Done(Err(err))) => {
                            ui.colored_label(ui.visuals().error_fg_color, format!("✖ {err}"));
                        }
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Debounce");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut self.hooks.debounce_ms)
                                .range(0..=60_000)
                                .speed(10)
                                .suffix(" ms"),
                        )
                        .on_hover_text("How long a connection change must last before hooks run")
                        .changed();
                });
            });
        });
        if changed {
            self.save();
        }
    }
}

pub fn run_window() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ggoled hooks")
            .with_app_id("se.ramse.ggoled")
            .with_inner_size([600.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("ggoled hooks", options, Box::new(|_cc| Ok(Box::new(HooksApp::new()))))
}
