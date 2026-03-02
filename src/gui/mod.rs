pub mod app;
pub mod pw_thread;

use std::sync::Arc;

use crate::config::Config;
use crate::pipewire_filter::RuntimeParams;

/// Role a user can assign to an audio app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppRole {
    None,
    Music,
    Voice,
}

/// An audio app discovered via PipeWire registry.
#[allow(dead_code)]
pub struct AudioApp {
    pub node_id: u32,
    pub name: String,
    pub role: AppRole,
    /// Output port IDs belonging to this node.
    pub port_ids: Vec<u32>,
}

/// GUI → PipeWire thread messages.
pub enum GuiToPw {
    SetRole { node_id: u32, role: AppRole },
    Quit,
}

/// PipeWire thread → GUI messages.
pub enum PwToGui {
    NodeAdded { node_id: u32, name: String },
    NodeRemoved { node_id: u32 },
    PortAdded { port_id: u32, node_id: u32 },
    PortRemoved { port_id: u32 },
    FilterReady,
    FilterState(String),
}

pub fn run_gui(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    // Create shared runtime parameters (GUI writes, RT thread reads)
    let runtime_params = Arc::new(RuntimeParams::new(config));

    // Create std::sync::mpsc channel (PW → GUI)
    let (gui_rx_tx, gui_rx) = std::sync::mpsc::channel::<PwToGui>();

    // Create pipewire channel (GUI → PW) — must be created on PW thread
    // since Receiver needs to attach to the PW loop.
    // We'll pass the config and gui_rx_tx into the PW thread, and get
    // the pw Sender back via a oneshot.
    let (sender_tx, sender_rx) =
        std::sync::mpsc::channel::<pipewire::channel::Sender<GuiToPw>>();

    let config_clone = config.clone();
    let params_for_pw = Arc::clone(&runtime_params);
    let pw_thread = std::thread::spawn(move || {
        pw_thread::run_pw_thread(config_clone, gui_rx_tx, sender_tx, params_for_pw);
    });

    // Wait for the PW thread to send us the Sender
    let pw_tx = sender_rx.recv().map_err(|_| "PipeWire thread failed to start")?;

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([480.0, 400.0])
            .with_min_inner_size([360.0, 200.0]),
        ..Default::default()
    };

    let config_for_gui = config.clone();
    eframe::run_native(
        "SpectralBlend",
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(app::GuiApp::new(pw_tx, gui_rx, config_for_gui, runtime_params)))
        }),
    )
    .map_err(|e| format!("eframe error: {e}"))?;

    // GUI closed — PW thread should have quit via on_close sending Quit
    let _ = pw_thread.join();
    Ok(())
}
