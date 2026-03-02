use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui;

use crate::config::Config;
use crate::dsp::mixer::db_to_gain;
use crate::pipewire_filter::RuntimeParams;

use super::{AppRole, AudioApp, GuiToPw, PwToGui};

pub struct GuiApp {
    apps: HashMap<u32, AudioApp>,
    filter_state: String,
    pw_tx: pipewire::channel::Sender<GuiToPw>,
    pw_rx: std::sync::mpsc::Receiver<PwToGui>,
    config: Config,
    params: Arc<RuntimeParams>,
    // Local slider state
    compression_pct: f32,  // 0-100
    voice_gain_db: f32,    // -12 to +12
    masking_pct: f32,      // 0-100
    wideband_pct: f32,     // 0-100
    controls_open: bool,
    // Remember app name → role for auto-reconnection
    remembered_roles: HashMap<String, AppRole>,
}

impl GuiApp {
    pub fn new(
        pw_tx: pipewire::channel::Sender<GuiToPw>,
        pw_rx: std::sync::mpsc::Receiver<PwToGui>,
        config: Config,
        params: Arc<RuntimeParams>,
    ) -> Self {
        Self {
            apps: HashMap::new(),
            filter_state: "Connecting".to_string(),
            pw_tx,
            pw_rx,
            compression_pct: 100.0,
            voice_gain_db: config.output.voice_gain_db,
            masking_pct: config.masking.depth * 100.0,
            wideband_pct: config.masking.focus_strength * 100.0,
            controls_open: true,
            config,
            params,
            remembered_roles: HashMap::new(),
        }
    }

    fn drain_pw_messages(&mut self) {
        // Collect auto-route requests to send after processing all messages
        let mut auto_routes: Vec<(u32, AppRole)> = Vec::new();

        while let Ok(msg) = self.pw_rx.try_recv() {
            match msg {
                PwToGui::NodeAdded { node_id, name } => {
                    // Check if we remember a role for this app name
                    let remembered = self.remembered_roles.get(&name).copied();
                    self.apps.entry(node_id).or_insert(AudioApp {
                        node_id,
                        name,
                        role: remembered.unwrap_or(AppRole::None),
                        port_ids: Vec::new(),
                    });
                }
                PwToGui::NodeRemoved { node_id } => {
                    self.apps.remove(&node_id);
                }
                PwToGui::PortAdded { port_id, node_id } => {
                    if let Some(app) = self.apps.get_mut(&node_id) {
                        if !app.port_ids.contains(&port_id) {
                            app.port_ids.push(port_id);
                        }
                        // Auto-route: if this app has a remembered role and now has
                        // enough ports (stereo pair), send the routing command
                        if app.role != AppRole::None && app.port_ids.len() == 2 {
                            auto_routes.push((node_id, app.role));
                        }
                    }
                }
                PwToGui::PortRemoved { port_id } => {
                    for app in self.apps.values_mut() {
                        app.port_ids.retain(|&id| id != port_id);
                    }
                }
                PwToGui::FilterReady => {}
                PwToGui::FilterState(s) => {
                    self.filter_state = s;
                }
            }
        }

        // Send auto-route commands for reconnected apps
        for (node_id, role) in auto_routes {
            let _ = self.pw_tx.send(GuiToPw::SetRole { node_id, role });
        }
    }

    fn state_color(&self) -> egui::Color32 {
        match self.filter_state.as_str() {
            "Streaming" => egui::Color32::from_rgb(80, 200, 80),
            "Paused" => egui::Color32::from_rgb(220, 180, 40),
            "Connecting" => egui::Color32::from_rgb(220, 180, 40),
            _ => egui::Color32::from_rgb(220, 60, 60),
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_pw_messages();

        // Top panel: title + filter state
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("SpectralBlend");
                ui.separator();
                let color = self.state_color();
                let dot = egui::RichText::new("●").color(color);
                ui.label(dot);
                ui.label(&self.filter_state);
            });
        });

        // Bottom panel: config summary
        egui::TopBottomPanel::bottom("bottom_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!(
                    "FFT: {} | Hop: {:.0}% | SR: {} Hz",
                    self.config.processing.fft_size,
                    self.config.processing.hop_ratio * 100.0,
                    self.config.processing.sample_rate,
                ));
            });
        });

        // Controls panel
        egui::TopBottomPanel::top("controls_panel").show(ctx, |ui| {
            ui.add_space(2.0);
            egui::CollapsingHeader::new("Controls")
                .default_open(self.controls_open)
                .show(ui, |ui| {
                    self.controls_open = true;
                    let slider_width = ui.available_width() - 100.0;

                    ui.horizontal(|ui| {
                        ui.label("Compression");
                        ui.add_sized(
                            [slider_width, 18.0],
                            egui::Slider::new(&mut self.compression_pct, 0.0..=100.0)
                                .suffix("%")
                                .fixed_decimals(0),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label("Voice Volume");
                        ui.add_sized(
                            [slider_width, 18.0],
                            egui::Slider::new(&mut self.voice_gain_db, -12.0..=12.0)
                                .suffix(" dB")
                                .fixed_decimals(1),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label("Masking");
                        ui.add_sized(
                            [slider_width, 18.0],
                            egui::Slider::new(&mut self.masking_pct, 0.0..=100.0)
                                .suffix("%")
                                .fixed_decimals(0),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label("Wideband");
                        ui.add_sized(
                            [slider_width, 18.0],
                            egui::Slider::new(&mut self.wideband_pct, 0.0..=100.0)
                                .suffix("%")
                                .fixed_decimals(0),
                        );
                    });
                });

            // Write slider values to atomics every frame
            self.params.store_compression_mix(self.compression_pct / 100.0);
            self.params.store_voice_gain(db_to_gain(self.voice_gain_db));
            self.params.store_masking_depth(self.masking_pct / 100.0);
            self.params.store_wideband(self.wideband_pct / 100.0);

            ui.add_space(2.0);
        });

        // Central panel: app list
        egui::CentralPanel::default().show(ctx, |ui| {
            // Sort apps by name for stable display order
            let mut app_ids: Vec<u32> = self
                .apps
                .iter()
                .filter(|(_, app)| !app.port_ids.is_empty())
                .map(|(&id, _)| id)
                .collect();
            app_ids.sort_by(|a, b| {
                let na = &self.apps[a].name;
                let nb = &self.apps[b].name;
                na.cmp(nb)
            });

            if app_ids.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No audio apps detected. Play some audio to see apps here.");
                });
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for &node_id in &app_ids {
                        let app = &self.apps[&node_id];
                        let current_role = app.role;
                        let app_name = app.name.clone();

                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(&app_name).strong(),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let mut changed_role = None;

                                if ui
                                    .selectable_label(current_role == AppRole::Voice, "Voice")
                                    .clicked()
                                {
                                    changed_role = Some(if current_role == AppRole::Voice {
                                        AppRole::None
                                    } else {
                                        AppRole::Voice
                                    });
                                }
                                if ui
                                    .selectable_label(current_role == AppRole::Music, "Music")
                                    .clicked()
                                {
                                    changed_role = Some(if current_role == AppRole::Music {
                                        AppRole::None
                                    } else {
                                        AppRole::Music
                                    });
                                }
                                if ui
                                    .selectable_label(current_role == AppRole::None, "None")
                                    .clicked()
                                {
                                    changed_role = Some(AppRole::None);
                                }

                                if let Some(role) = changed_role {
                                    let _ = self.pw_tx.send(GuiToPw::SetRole { node_id, role });
                                    if let Some(app) = self.apps.get_mut(&node_id) {
                                        app.role = role;
                                        // Remember/forget role by app name for auto-reconnection
                                        if role != AppRole::None {
                                            self.remembered_roles.insert(app.name.clone(), role);
                                        } else {
                                            self.remembered_roles.remove(&app.name);
                                        }
                                    }
                                }
                            });
                        });
                        ui.separator();
                    }
                });
            }
        });

        // Request repaint periodically to pick up PW messages
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.pw_tx.send(GuiToPw::Quit);
    }
}
