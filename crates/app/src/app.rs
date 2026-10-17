//! Main window: engine/INDI connection bar, equipment roles, device browser,
//! property editor, log. All state comes from the engine over [`EngineLink`].

use std::collections::HashMap;

use eframe::egui::{self, Color32, RichText};
use indi_link::{Device, IndexMap, PropertyState, interface};
use protocol::{IndiStatus, LogEntry, LogLevel, Roles, ToEngine, ToUi, UiState};
use serde::{Deserialize, Serialize};

use crate::imaging_view::{CaptureForm, ImagingView};
use crate::link::{EngineLink, Incoming, LinkState};
use crate::property_view::{self, Edits};
use crate::sequence_view::{SequenceDraft, SequenceView};

/// Which engine the UI talks to (native builds only; the web UI always uses
/// the engine that served it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EngineChoice {
    /// Engine embedded in this process.
    Local,
    /// `host[:port]` or `ws://` URL of a standalone engine.
    Remote(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum View {
    Imaging,
    Sequence,
    Devices,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct UiSettings {
    night_mode: bool,
    engine: EngineChoice,
    view: View,
    capture: CaptureForm,
    sequence: SequenceDraft,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            night_mode: false,
            engine: EngineChoice::Local,
            view: View::Imaging,
            capture: CaptureForm::default(),
            sequence: SequenceDraft::default(),
        }
    }
}

/// Startup options from the command line.
#[derive(Debug, Default)]
pub struct StartOptions {
    pub engine: Option<EngineChoice>,
    /// INDI server for the embedded engine to connect to on start.
    pub local_indi: Option<(String, u16)>,
}

#[cfg(not(target_arch = "wasm32"))]
struct LocalEngine {
    runtime: tokio::runtime::Runtime,
    handle: Option<engine::EngineHandle>,
    indi: Option<(String, u16)>,
}

#[cfg(not(target_arch = "wasm32"))]
impl LocalEngine {
    fn ws_url(&mut self) -> Result<String, String> {
        if self.handle.is_none() {
            let config = engine::EngineConfig {
                listen: ([127, 0, 0, 1], 0).into(),
                web_root: None,
                state_file: engine::default_state_file(),
                indi: self.indi.take(),
            };
            let handle = self
                .runtime
                .block_on(engine::start(config))
                .map_err(|e| format!("could not start local engine: {e}"))?;
            self.handle = Some(handle);
        }
        Ok(self.handle.as_ref().expect("started above").ws_url())
    }

    fn stop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.shutdown();
        }
    }
}

pub struct App {
    settings: UiSettings,
    link: Option<EngineLink>,
    link_error: Option<String>,
    state: UiState,
    selected_device: Option<String>,
    selected_group: HashMap<String, String>,
    edits: Edits,
    indi_host: String,
    indi_port: u16,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    engine_input: String,
    applied_night_mode: Option<bool>,
    imaging: ImagingView,
    sequence: SequenceView,
    #[cfg(not(target_arch = "wasm32"))]
    local: LocalEngine,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, options: StartOptions) -> Self {
        let mut settings: UiSettings = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        if let Some(engine) = options.engine {
            settings.engine = engine;
        }
        let engine_input = match &settings.engine {
            EngineChoice::Remote(addr) => addr.clone(),
            EngineChoice::Local => String::new(),
        };
        #[cfg(target_arch = "wasm32")]
        let _ = options.local_indi;
        let imaging = ImagingView::new(settings.capture.clone());
        let sequence = SequenceView::new(settings.sequence.clone());
        let mut app = Self {
            settings,
            imaging,
            sequence,
            link: None,
            link_error: None,
            state: UiState::default(),
            selected_device: None,
            selected_group: HashMap::new(),
            edits: Edits::default(),
            indi_host: "localhost".into(),
            indi_port: indi_link::DEFAULT_PORT,
            engine_input,
            applied_night_mode: None,
            #[cfg(not(target_arch = "wasm32"))]
            local: LocalEngine {
                runtime: tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .thread_name("engine")
                    .build()
                    .expect("failed to start tokio runtime"),
                handle: None,
                indi: options.local_indi,
            },
        };
        app.connect_engine(&cc.egui_ctx);
        app
    }

    fn connect_engine(&mut self, ctx: &egui::Context) {
        self.link = None;
        self.state = UiState::default();
        match self.engine_url() {
            Ok(url) => {
                self.link_error = None;
                self.link = Some(EngineLink::new(url, ctx));
            }
            Err(e) => self.link_error = Some(e),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn engine_url(&mut self) -> Result<String, String> {
        match &self.settings.engine {
            EngineChoice::Local => self.local.ws_url(),
            EngineChoice::Remote(addr) => {
                self.local.stop();
                Ok(protocol::engine_url(addr))
            }
        }
    }

    /// The web UI talks to the engine that served the page.
    #[cfg(target_arch = "wasm32")]
    fn engine_url(&mut self) -> Result<String, String> {
        let location = web_sys::window().ok_or("no window")?.location();
        let host = location.host().map_err(|_| "no host")?;
        let secure = location.protocol().is_ok_and(|p| p == "https:");
        let scheme = if secure { "wss" } else { "ws" };
        Ok(format!("{scheme}://{host}{}", protocol::WS_PATH))
    }

    fn engine_label(&self) -> String {
        if cfg!(target_arch = "wasm32") {
            return "Engine".into();
        }
        match &self.settings.engine {
            EngineChoice::Local => "Local engine".into(),
            EngineChoice::Remote(_) => self
                .link
                .as_ref()
                .map(|l| l.url.clone())
                .unwrap_or_default(),
        }
    }

    fn local_log(&mut self, text: impl Into<String>) {
        self.state.push_log(LogEntry {
            time: String::new(),
            device: None,
            text: text.into(),
            level: LogLevel::Error,
        });
    }

    fn poll_link(&mut self, ctx: &egui::Context) {
        let Some(link) = &mut self.link else {
            return;
        };
        for incoming in link.poll() {
            let msg = match incoming {
                Incoming::Msg(msg) => msg,
                Incoming::Preview { id, jpeg } => {
                    self.imaging.on_preview(ctx, id, &jpeg);
                    continue;
                }
            };
            match &msg {
                ToUi::IndiStatus { host, port, .. } => {
                    self.indi_host.clone_from(host);
                    self.indi_port = *port;
                }
                ToUi::ImagingSettings(s) => self.imaging.on_settings(s),
                _ => {}
            }
            self.state.apply(msg);
        }
    }

    fn send_all(&mut self, actions: Vec<ToEngine>) {
        let Some(link) = &mut self.link else {
            return;
        };
        if let Some(err) = actions.iter().find_map(|a| link.send(a).err()) {
            self.local_log(err);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui, actions: &mut Vec<ToEngine>) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("indicontrol").strong());
            ui.separator();

            // Engine link.
            let (state, hover) = match self.link.as_ref().map(|l| &l.state) {
                Some(LinkState::Open) => (PropertyState::Ok, "connected".to_owned()),
                Some(LinkState::Connecting) => (PropertyState::Busy, "connecting…".to_owned()),
                Some(LinkState::Closed(e)) => (PropertyState::Alert, format!("{e} — retrying")),
                None => (
                    PropertyState::Alert,
                    self.link_error.clone().unwrap_or_default(),
                ),
            };
            property_view::state_dot(ui, state).on_hover_text(&hover);
            ui.label(self.engine_label()).on_hover_text(&hover);
            #[cfg(not(target_arch = "wasm32"))]
            self.engine_menu(ui);
            ui.separator();

            // INDI server, as seen by the engine.
            let usable = self.link.as_ref().is_some_and(EngineLink::is_open);
            ui.add_enabled_ui(usable, |ui| {
                ui.label("INDI");
                let disconnected =
                    matches!(self.state.indi_status, IndiStatus::Disconnected { .. });
                ui.add_enabled(
                    disconnected,
                    egui::TextEdit::singleline(&mut self.indi_host).desired_width(140.0),
                );
                ui.add_enabled(
                    disconnected,
                    egui::DragValue::new(&mut self.indi_port).range(1..=65535),
                );
                match &self.state.indi_status {
                    IndiStatus::Disconnected { error } => {
                        if ui.button("Connect").clicked() {
                            actions.push(ToEngine::ConnectIndi {
                                host: self.indi_host.trim().to_owned(),
                                port: self.indi_port,
                            });
                        }
                        if let Some(e) = error {
                            ui.label(RichText::new(e).color(Color32::from_rgb(220, 60, 60)));
                        }
                    }
                    IndiStatus::Connecting => {
                        ui.add(egui::Spinner::new());
                        if ui.button("Cancel").clicked() {
                            actions.push(ToEngine::DisconnectIndi);
                        }
                    }
                    IndiStatus::Connected => {
                        if ui.button("Disconnect").clicked() {
                            actions.push(ToEngine::DisconnectIndi);
                        }
                        ui.label(
                            RichText::new(format!("{} devices", self.state.cache.devices.len()))
                                .weak(),
                        );
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.settings.night_mode, "Night mode");
            });
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn engine_menu(&mut self, ui: &mut egui::Ui) {
        let mut apply = None;
        ui.menu_button("Change…", |ui| {
            if ui
                .radio(
                    self.settings.engine == EngineChoice::Local,
                    "Local engine (inside this app)",
                )
                .clicked()
            {
                apply = Some(EngineChoice::Local);
            }
            ui.label("Remote engine:");
            ui.horizontal(|ui| {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.engine_input)
                        .hint_text("host[:port]")
                        .desired_width(180.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let valid = !self.engine_input.trim().is_empty();
                let clicked = ui
                    .add_enabled(valid, egui::Button::new("Connect"))
                    .clicked();
                if valid && (clicked || enter) {
                    apply = Some(EngineChoice::Remote(self.engine_input.trim().to_owned()));
                }
            });
            if apply.is_some() {
                ui.close();
            }
        });
        if let Some(choice) = apply {
            self.settings.engine = choice;
            self.connect_engine(ui.ctx());
        }
    }

    fn equipment_panel(&mut self, ui: &mut egui::Ui, actions: &mut Vec<ToEngine>) {
        ui.heading("Equipment");
        let mut roles = self.state.roles.clone();
        egui::Grid::new("roles").num_columns(2).show(ui, |ui| {
            let devices = &self.state.cache.devices;
            for (label, slot, flag) in role_slots(&mut roles) {
                ui.label(label);
                role_combo(ui, label, slot, devices, flag);
                ui.end_row();
            }
        });
        if roles != self.state.roles {
            self.state.roles = roles.clone();
            actions.push(ToEngine::SetRoles(roles));
        }

        ui.separator();
        ui.heading("Devices");
        if self.state.cache.devices.is_empty() {
            ui.label(RichText::new("No devices — connect to an INDI server.").weak());
        }
        egui::ScrollArea::vertical().id_salt("devices").show(ui, |ui| {
            for dev in self.state.cache.devices.values() {
                ui.horizontal(|ui| {
                    let connected = dev.is_connected();
                    property_view::state_dot(
                        ui,
                        if connected {
                            PropertyState::Ok
                        } else {
                            PropertyState::Idle
                        },
                    );
                    let selected = self.selected_device.as_deref() == Some(dev.name.as_str());
                    if ui.selectable_label(selected, &dev.name).clicked() {
                        self.selected_device = Some(dev.name.clone());
                    }
                    if dev.properties.contains_key("CONNECTION") {
                        let device = dev.name.clone();
                        if connected {
                            if ui.small_button("Disconnect").clicked() {
                                actions.push(ToEngine::DisconnectDevice { device });
                            }
                        } else if ui.small_button("Connect").clicked() {
                            actions.push(ToEngine::ConnectDevice { device });
                        }
                    }
                });
                let kinds = device_kinds(dev);
                if !kinds.is_empty() {
                    ui.label(RichText::new(kinds).small().weak());
                }
            }
        });
    }

    fn device_view(&mut self, ui: &mut egui::Ui, actions: &mut Vec<ToEngine>) {
        if let Some(v) = self.state.version_mismatch {
            ui.colored_label(
                Color32::from_rgb(230, 160, 40),
                format!(
                    "Engine speaks protocol v{v}, this UI v{} — update one of them.",
                    protocol::PROTOCOL_VERSION
                ),
            );
        }
        let Some(dev) = self
            .selected_device
            .as_ref()
            .and_then(|name| self.state.cache.devices.get(name))
        else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Select a device to view its properties.").weak());
            });
            return;
        };

        ui.horizontal(|ui| {
            ui.heading(&dev.name);
            if let Some(driver) = dev
                .properties
                .get("DRIVER_INFO")
                .and_then(|p| p.text("DRIVER_EXEC"))
            {
                ui.label(RichText::new(driver).weak());
            }
        });

        let groups = dev.groups();
        let current = self
            .selected_group
            .get(&dev.name)
            .filter(|g| groups.contains(&g.as_str()))
            .cloned()
            .or_else(|| groups.first().map(|g| g.to_string()))
            .unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            for g in &groups {
                let label = if g.is_empty() { "(none)" } else { g };
                if ui.selectable_label(*g == current, label).clicked() {
                    self.selected_group.insert(dev.name.clone(), g.to_string());
                }
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt(("props", &dev.name, &current))
            .auto_shrink(false)
            .show(ui, |ui| {
                for p in dev.properties.values().filter(|p| p.group == current) {
                    property_view::show_property(ui, p, &mut self.edits, actions);
                }
            });
    }

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Log").strong());
            if ui.small_button("Clear").clicked() {
                self.state.log.clear();
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("log")
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                for entry in &self.state.log {
                    let mut text = format!("{} ", entry.time);
                    if let Some(d) = &entry.device {
                        text += &format!("[{d}] ");
                    }
                    text += &entry.text;
                    let mut rt = RichText::new(text).monospace();
                    match entry.level {
                        LogLevel::Error => rt = rt.color(Color32::from_rgb(220, 60, 60)),
                        LogLevel::Warning => rt = rt.color(Color32::from_rgb(230, 160, 40)),
                        LogLevel::Info => {}
                    }
                    ui.label(rt);
                }
            });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_link(ctx);
        if self.selected_device.is_none() {
            self.selected_device = self.state.cache.devices.keys().next().cloned();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.applied_night_mode != Some(self.settings.night_mode) {
            apply_theme(ui.ctx(), self.settings.night_mode);
            self.applied_night_mode = Some(self.settings.night_mode);
        }
        let usable = self.link.as_ref().is_some_and(EngineLink::is_open);
        let mut actions = Vec::new();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui, &mut actions));
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(160.0)
            .show(ui, |ui| self.log_panel(ui));
        egui::Panel::left("equipment")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| {
                ui.add_enabled_ui(usable, |ui| self.equipment_panel(ui, &mut actions));
            });
        egui::CentralPanel::default().show(ui, |ui| {
            if !usable {
                ui.colored_label(
                    Color32::from_rgb(230, 160, 40),
                    "Not connected to the engine.",
                );
            }
            ui.horizontal(|ui| {
                for (view, label) in [
                    (View::Imaging, "Imaging"),
                    (View::Sequence, "Sequence"),
                    (View::Devices, "Devices"),
                ] {
                    ui.selectable_value(&mut self.settings.view, view, RichText::new(label).heading());
                }
                if let Some(s) = &self.state.sequence
                    && self.settings.view != View::Sequence
                {
                    ui.label(
                        RichText::new(format!(
                            "Sequence: {} / {} frames",
                            s.frames_done(),
                            s.sequence.planned_frames()
                        ))
                        .weak(),
                    );
                }
            });
            ui.separator();
            ui.add_enabled_ui(usable, |ui| match self.settings.view {
                View::Imaging => self.imaging.ui(ui, &self.state, &mut actions),
                View::Sequence => self.sequence.ui(ui, &self.state, &mut self.imaging, &mut actions),
                View::Devices => self.device_view(ui, &mut actions),
            });
        });
        if !actions.is_empty() {
            self.send_all(actions);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.settings.capture = self.imaging.form.clone();
        self.settings.sequence = self.sequence.draft.clone();
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
}

fn role_slots(roles: &mut Roles) -> [(&'static str, &mut Option<String>, u32); 5] {
    [
        ("Main camera", &mut roles.main_camera, interface::CCD),
        ("Guide camera", &mut roles.guide_camera, interface::CCD),
        ("Mount", &mut roles.mount, interface::TELESCOPE),
        ("Filter wheel", &mut roles.filter_wheel, interface::FILTER),
        ("Focuser", &mut roles.focuser, interface::FOCUSER),
    ]
}

fn role_combo(
    ui: &mut egui::Ui,
    id: &str,
    slot: &mut Option<String>,
    devices: &IndexMap<String, Device>,
    flag: u32,
) {
    let candidates: Vec<&str> = devices
        .values()
        // Devices that have not reported interfaces yet are offered too.
        .filter(|d| d.interfaces() == 0 || d.has_interface(flag))
        .map(|d| d.name.as_str())
        .collect();
    let mut text = slot.clone().unwrap_or_else(|| "—".into());
    if slot.as_ref().is_some_and(|s| !devices.contains_key(s)) {
        text += " (offline)";
    }
    egui::ComboBox::from_id_salt(id)
        .selected_text(text)
        .width(170.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(slot, None, "—");
            for name in candidates {
                ui.selectable_value(slot, Some(name.to_owned()), name);
            }
        });
}

/// Human-readable list of driver interfaces, e.g. "Camera · Filter wheel".
fn device_kinds(dev: &Device) -> String {
    let i = dev.interfaces();
    [
        (interface::TELESCOPE, "Mount"),
        (interface::CCD, "Camera"),
        (interface::GUIDER, "Guider"),
        (interface::FOCUSER, "Focuser"),
        (interface::FILTER, "Filter wheel"),
        (interface::DOME, "Dome"),
        (interface::GPS, "GPS"),
        (interface::WEATHER, "Weather"),
        (interface::AO, "AO"),
        (interface::DUSTCAP, "Dust cap"),
        (interface::LIGHTBOX, "Light box"),
        (interface::ROTATOR, "Rotator"),
    ]
    .iter()
    .filter(|(flag, _)| i & flag != 0)
    .map(|(_, name)| *name)
    .collect::<Vec<_>>()
    .join(" · ")
}

/// Dark theme, or an all-red night theme that preserves dark adaptation.
fn apply_theme(ctx: &egui::Context, night: bool) {
    let mut v = egui::Visuals::dark();
    if night {
        let red = |r: u8| Color32::from_rgb(r, 0, 0);
        v.override_text_color = Some(Color32::from_rgb(200, 30, 20));
        v.panel_fill = red(14);
        v.window_fill = red(14);
        v.extreme_bg_color = red(6);
        v.faint_bg_color = red(20);
        v.hyperlink_color = red(220);
        v.selection.bg_fill = red(90);
        v.selection.stroke.color = red(220);
        for (w, bg) in [
            (&mut v.widgets.noninteractive, 18),
            (&mut v.widgets.inactive, 35),
            (&mut v.widgets.hovered, 55),
            (&mut v.widgets.active, 75),
            (&mut v.widgets.open, 45),
        ] {
            w.bg_fill = red(bg);
            w.weak_bg_fill = red(bg);
            w.bg_stroke.color = red(bg + 30);
            w.fg_stroke.color = red(200);
        }
    }
    ctx.set_visuals(v);
}
