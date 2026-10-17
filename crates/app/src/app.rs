//! Main window: connection bar, equipment roles, device browser, property editor, log.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, mpsc as std_mpsc};

use eframe::egui::{self, Color32, RichText};
use indi_link::{Cache, Client, ClientError, Device, Event, IndexMap, interface};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::property_view::{self, Action, Edits, human_bytes};
use crate::settings::Settings;

const MAX_LOG: usize = 5000;

type ConnectResult = Result<(Client, UnboundedReceiver<Event>), ClientError>;

enum Connection {
    Disconnected,
    Connecting(std_mpsc::Receiver<ConnectResult>),
    Connected {
        client: Client,
        events: UnboundedReceiver<Event>,
    },
}

struct LogEntry {
    time: String,
    device: Option<String>,
    text: String,
    error: bool,
}

pub struct App {
    runtime: tokio::runtime::Runtime,
    settings: Settings,
    connection: Connection,
    /// Copy of the client cache, refreshed whenever the server changes something.
    snapshot: Cache,
    log: VecDeque<LogEntry>,
    selected_device: Option<String>,
    selected_group: HashMap<String, String>,
    edits: Edits,
    applied_night_mode: Option<bool>,
    connect_on_start: bool,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        runtime: tokio::runtime::Runtime,
        autoconnect: Option<(String, u16)>,
    ) -> Self {
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        let connect_on_start = autoconnect.is_some();
        if let Some((host, port)) = autoconnect {
            settings.host = host;
            settings.port = port;
        }
        Self {
            runtime,
            settings,
            connection: Connection::Disconnected,
            snapshot: Cache::default(),
            log: VecDeque::new(),
            selected_device: None,
            selected_group: HashMap::new(),
            edits: Edits::default(),
            applied_night_mode: None,
            connect_on_start,
        }
    }

    fn log(&mut self, device: Option<String>, text: impl Into<String>, error: bool) {
        if self.log.len() >= MAX_LOG {
            self.log.pop_front();
        }
        self.log.push_back(LogEntry {
            time: chrono::Local::now().format("%H:%M:%S").to_string(),
            device,
            text: text.into(),
            error,
        });
    }

    fn start_connect(&mut self, ctx: &egui::Context) {
        let addr = (self.settings.host.trim().to_owned(), self.settings.port);
        let (tx, rx) = std_mpsc::channel();
        let ctx = ctx.clone();
        let waker_ctx = ctx.clone();
        self.runtime.spawn(async move {
            let waker = Arc::new(move || waker_ctx.request_repaint());
            let _ = tx.send(Client::connect_with_waker(addr, waker).await);
            ctx.request_repaint();
        });
        self.log(
            None,
            format!("Connecting to {}:{}…", self.settings.host, self.settings.port),
            false,
        );
        self.connection = Connection::Connecting(rx);
    }

    fn disconnect(&mut self) {
        if let Connection::Connected { client, .. } = &self.connection {
            client.disconnect();
            self.log(None, "Disconnected", false);
        }
        self.connection = Connection::Disconnected;
        self.snapshot.clear();
    }

    fn poll_connection(&mut self) {
        let Connection::Connecting(rx) = &self.connection else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((client, events))) => {
                self.log(
                    None,
                    format!("Connected to {}:{}", self.settings.host, self.settings.port),
                    false,
                );
                self.connection = Connection::Connected { client, events };
            }
            Ok(Err(e)) => {
                self.log(None, e.to_string(), true);
                self.connection = Connection::Disconnected;
            }
            Err(std_mpsc::TryRecvError::Empty) => {}
            Err(std_mpsc::TryRecvError::Disconnected) => {
                self.connection = Connection::Disconnected;
            }
        }
    }

    fn poll_events(&mut self) {
        let Connection::Connected { client, events } = &mut self.connection else {
            return;
        };
        let mut received = Vec::new();
        while let Ok(ev) = events.try_recv() {
            received.push(ev);
        }
        if received.is_empty() {
            return;
        }
        self.snapshot = client.cache().clone();
        let mut lost = None;
        for ev in received {
            match ev {
                Event::Message { device, text, .. } => self.log(device, text, false),
                Event::Blob {
                    device,
                    element,
                    format,
                    data,
                    ..
                } => self.log(
                    Some(device),
                    format!("Received {element} ({format}, {})", human_bytes(data.len())),
                    false,
                ),
                Event::Disconnected(reason) => lost = Some(reason),
                Event::Defined { .. } | Event::Updated { .. } | Event::Deleted { .. } => {}
            }
        }
        if let Some(reason) = lost {
            let text = match reason {
                Some(r) => format!("Connection lost: {r}"),
                None => "Server closed the connection".to_owned(),
            };
            self.log(None, text, true);
            self.connection = Connection::Disconnected;
            self.snapshot.clear();
        }
    }

    fn execute(&mut self, actions: Vec<Action>) {
        let Connection::Connected { client, .. } = &self.connection else {
            return;
        };
        let mut errors = Vec::new();
        for action in actions {
            let result = match &action {
                Action::ConnectDevice(d) => client.connect_device(d),
                Action::DisconnectDevice(d) => client.disconnect_device(d),
                Action::Text {
                    device,
                    name,
                    values,
                } => {
                    let v: Vec<(&str, &str)> =
                        values.iter().map(|(e, v)| (e.as_str(), v.as_str())).collect();
                    client.send_text(device, name, &v)
                }
                Action::Number {
                    device,
                    name,
                    values,
                } => {
                    let v: Vec<(&str, f64)> = values.iter().map(|(e, v)| (e.as_str(), *v)).collect();
                    client.send_number(device, name, &v)
                }
                Action::Switch {
                    device,
                    name,
                    values,
                } => {
                    let v: Vec<(&str, bool)> = values.iter().map(|(e, v)| (e.as_str(), *v)).collect();
                    client.send_switch(device, name, &v)
                }
            };
            if let Err(e) = result {
                errors.push(e.to_string());
            }
        }
        for e in errors {
            self.log(None, e, true);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("indicontrol").strong());
            ui.separator();
            let editable = matches!(self.connection, Connection::Disconnected);
            ui.label("Server");
            ui.add_enabled(
                editable,
                egui::TextEdit::singleline(&mut self.settings.host).desired_width(160.0),
            );
            ui.add_enabled(
                editable,
                egui::DragValue::new(&mut self.settings.port).range(1..=65535),
            );
            match &self.connection {
                Connection::Disconnected => {
                    if ui.button("Connect").clicked() {
                        self.start_connect(ui.ctx());
                    }
                }
                Connection::Connecting(_) => {
                    ui.add(egui::Spinner::new());
                    ui.label("Connecting…");
                }
                Connection::Connected { .. } => {
                    if ui.button("Disconnect").clicked() {
                        self.disconnect();
                    }
                    ui.label(
                        RichText::new(format!("{} devices", self.snapshot.devices.len())).weak(),
                    );
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.checkbox(&mut self.settings.night_mode, "Night mode");
            });
        });
    }

    fn equipment_panel(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        ui.heading("Equipment");
        egui::Grid::new("roles").num_columns(2).show(ui, |ui| {
            let devices = &self.snapshot.devices;
            let roles = &mut self.settings.roles;
            for (label, slot, flag) in [
                ("Main camera", &mut roles.main_camera, interface::CCD),
                ("Guide camera", &mut roles.guide_camera, interface::CCD),
                ("Mount", &mut roles.mount, interface::TELESCOPE),
                ("Filter wheel", &mut roles.filter_wheel, interface::FILTER),
                ("Focuser", &mut roles.focuser, interface::FOCUSER),
            ] {
                ui.label(label);
                role_combo(ui, label, slot, devices, flag);
                ui.end_row();
            }
        });

        ui.separator();
        ui.heading("Devices");
        if self.snapshot.devices.is_empty() {
            ui.label(RichText::new("No devices — connect to an INDI server.").weak());
        }
        egui::ScrollArea::vertical().id_salt("devices").show(ui, |ui| {
            for dev in self.snapshot.devices.values() {
                ui.horizontal(|ui| {
                    let connected = dev.is_connected();
                    property_view::state_dot(
                        ui,
                        if connected {
                            indi_link::PropertyState::Ok
                        } else {
                            indi_link::PropertyState::Idle
                        },
                    );
                    let selected = self.selected_device.as_deref() == Some(dev.name.as_str());
                    if ui.selectable_label(selected, &dev.name).clicked() {
                        self.selected_device = Some(dev.name.clone());
                    }
                    if dev.properties.contains_key("CONNECTION") {
                        let (text, action) = if connected {
                            ("Disconnect", Action::DisconnectDevice(dev.name.clone()))
                        } else {
                            ("Connect", Action::ConnectDevice(dev.name.clone()))
                        };
                        if ui.small_button(text).clicked() {
                            actions.push(action);
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

    fn device_view(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let Some(dev) = self
            .selected_device
            .as_ref()
            .and_then(|name| self.snapshot.devices.get(name))
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
                self.log.clear();
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("log")
            .stick_to_bottom(true)
            .auto_shrink(false)
            .show(ui, |ui| {
                for entry in &self.log {
                    let mut text = format!("{} ", entry.time);
                    if let Some(d) = &entry.device {
                        text += &format!("[{d}] ");
                    }
                    text += &entry.text;
                    let mut rt = RichText::new(text).monospace();
                    if entry.error {
                        rt = rt.color(Color32::from_rgb(220, 60, 60));
                    }
                    ui.label(rt);
                }
            });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.connect_on_start) {
            self.start_connect(ctx);
        }
        self.poll_connection();
        self.poll_events();
        if self.selected_device.is_none() {
            self.selected_device = self.snapshot.devices.keys().next().cloned();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.applied_night_mode != Some(self.settings.night_mode) {
            apply_theme(ui.ctx(), self.settings.night_mode);
            self.applied_night_mode = Some(self.settings.night_mode);
        }
        let mut actions = Vec::new();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(160.0)
            .show(ui, |ui| self.log_panel(ui));
        egui::Panel::left("equipment")
            .resizable(true)
            .default_size(280.0)
            .show(ui, |ui| self.equipment_panel(ui, &mut actions));
        egui::CentralPanel::default().show(ui, |ui| self.device_view(ui, &mut actions));
        if !actions.is_empty() {
            self.execute(actions);
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }
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

