use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::rc::Rc;

use pipewire as pw;
use pw::main_loop::MainLoopRc;
use pw::properties::properties;
use pw::sys as pw_sys;
use pw::types::ObjectType;

use std::sync::Arc;

use crate::config::Config;
use crate::pipewire_filter::{
    add_port, filter_state_name, on_process, on_state_changed, FilterState, RuntimeParams,
};

use super::{AppRole, GuiToPw, PwToGui};

// ---------------------------------------------------------------------------
// Port name classification helpers
// ---------------------------------------------------------------------------

fn is_left_port(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("_fl") || n.contains("left") || n.contains("mono")
}

fn is_right_port(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("_fr") || n.contains("right")
}

fn classify_stereo_ports(ports: &[(u32, &str)]) -> (Option<u32>, Option<u32>) {
    let mut left = None;
    let mut right = None;

    for &(id, name) in ports {
        if is_left_port(name) && left.is_none() {
            left = Some(id);
        } else if is_right_port(name) && right.is_none() {
            right = Some(id);
        }
    }

    if left.is_none() && right.is_none() && ports.len() >= 2 {
        let mut sorted: Vec<u32> = ports.iter().map(|(id, _)| *id).collect();
        sorted.sort();
        left = Some(sorted[0]);
        right = Some(sorted[1]);
    } else if left.is_none() && right.is_none() && ports.len() == 1 {
        left = Some(ports[0].0);
    }

    (left, right)
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

struct TrackedLink {
    output_node_id: u32,
    input_node_id: u32,
}

struct PwState {
    nodes: HashMap<u32, NodeInfo>,
    ports: HashMap<u32, PortInfo>,

    filter_node_id: Option<u32>,
    filter_input_ports: HashMap<String, u32>,
    filter_output_ports: HashMap<String, u32>,

    sink_node_id: Option<u32>,
    sink_input_ports: HashMap<u32, String>,

    active_links: HashMap<u32, Vec<pw::link::Link>>,
    /// Links created when un-routing (app → sink), kept alive until re-routed.
    restore_links: HashMap<u32, Vec<pw::link::Link>>,
    existing_links: HashMap<u32, TrackedLink>,
    routed_nodes: std::collections::HashSet<u32>,

    output_links: Vec<pw::link::Link>,
}

struct NodeInfo {
    #[allow(dead_code)]
    name: String,
}

struct PortInfo {
    node_id: u32,
    name: String,
    direction: String,
}

// ---------------------------------------------------------------------------
// Filter wrapper for GUI (state-change notifications + debug recording)
// ---------------------------------------------------------------------------

#[repr(C)]
struct FilterAndGui {
    filter: FilterState,
    gui_tx: std::sync::mpsc::Sender<PwToGui>,
}

unsafe extern "C" fn gui_on_state_changed(
    data: *mut c_void,
    old: pw_sys::pw_filter_state,
    new: pw_sys::pw_filter_state,
    error: *const c_char,
) {
    on_state_changed(data, old, new, error);
    let fg = &*(data as *const FilterAndGui);
    let name = filter_state_name(new).to_string();
    let _ = fg.gui_tx.send(PwToGui::FilterState(name));
}

// ---------------------------------------------------------------------------
// Link creation helper
// ---------------------------------------------------------------------------

fn create_link(
    core: &pw::core::Core,
    out_node: u32,
    out_port: u32,
    in_node: u32,
    in_port: u32,
) -> Option<pw::link::Link> {
    create_link_inner(core, out_node, out_port, in_node, in_port, false)
}

fn create_lingering_link(
    core: &pw::core::Core,
    out_node: u32,
    out_port: u32,
    in_node: u32,
    in_port: u32,
) -> Option<pw::link::Link> {
    create_link_inner(core, out_node, out_port, in_node, in_port, true)
}

fn create_link_inner(
    core: &pw::core::Core,
    out_node: u32,
    out_port: u32,
    in_node: u32,
    in_port: u32,
    linger: bool,
) -> Option<pw::link::Link> {
    let out_node_s = out_node.to_string();
    let out_port_s = out_port.to_string();
    let in_node_s = in_node.to_string();
    let in_port_s = in_port.to_string();

    let mut link_props = properties! {
        "link.output.node" => out_node_s.as_str(),
        "link.output.port" => out_port_s.as_str(),
        "link.input.node" => in_node_s.as_str(),
        "link.input.port" => in_port_s.as_str()
    };
    if linger {
        link_props.insert("object.linger", "true");
    }

    match core.create_object::<pw::link::Link>("link-factory", &link_props) {
        Ok(link) => {
            eprintln!(
                "  [route]   Link created: port {} -> port {}",
                out_port, in_port
            );
            Some(link)
        }
        Err(e) => {
            eprintln!(
                "  [route]   Link FAILED: {} -> {}: {:?}",
                out_port, in_port, e
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: link filter output to sink
// ---------------------------------------------------------------------------

fn try_link_filter_output(state: &mut PwState, core: &pw::core::Core) {
    if !state.output_links.is_empty() {
        return;
    }

    let filter_node_id = match state.filter_node_id {
        Some(id) => id,
        None => return,
    };
    let sink_node_id = match state.sink_node_id {
        Some(id) => id,
        None => return,
    };

    let out_left = match state.filter_output_ports.get("output_left") {
        Some(&id) => id,
        None => return,
    };
    let out_right = match state.filter_output_ports.get("output_right") {
        Some(&id) => id,
        None => return,
    };

    if state.sink_input_ports.is_empty() {
        return;
    }

    let sink_ports: Vec<(u32, &str)> = state
        .sink_input_ports
        .iter()
        .map(|(&id, name)| (id, name.as_str()))
        .collect();
    let (sink_left, sink_right) = classify_stereo_ports(&sink_ports);

    let sink_left = match sink_left {
        Some(id) => id,
        None => return,
    };
    let sink_right = sink_right.unwrap_or(sink_left);

    eprintln!(
        "  [output] Linking filter output to sink: filter={} -> sink={}",
        filter_node_id, sink_node_id
    );

    if let Some(link) = create_link(core, filter_node_id, out_left, sink_node_id, sink_left) {
        state.output_links.push(link);
    }
    if let Some(link) = create_link(core, filter_node_id, out_right, sink_node_id, sink_right) {
        state.output_links.push(link);
    }
}

// ---------------------------------------------------------------------------
// Helper: create links from app output ports to sink (returns Link proxies)
// ---------------------------------------------------------------------------

fn create_sink_links(
    state: &PwState,
    core: &pw::core::Core,
    node_id: u32,
    linger: bool,
) -> Vec<pw::link::Link> {
    let sink_node_id = match state.sink_node_id {
        Some(id) => id,
        None => return Vec::new(),
    };

    if state.sink_input_ports.is_empty() {
        return Vec::new();
    }

    let app_ports: Vec<(u32, String)> = state
        .ports
        .iter()
        .filter(|(_, p)| p.node_id == node_id && p.direction == "out")
        .map(|(&id, p)| (id, p.name.clone()))
        .collect();

    if app_ports.is_empty() {
        return Vec::new();
    }

    let app_port_refs: Vec<(u32, &str)> = app_ports.iter().map(|(id, n)| (*id, n.as_str())).collect();
    let (app_left, app_right) = classify_stereo_ports(&app_port_refs);

    let sink_port_refs: Vec<(u32, &str)> = state
        .sink_input_ports
        .iter()
        .map(|(&id, name)| (id, name.as_str()))
        .collect();
    let (sink_left, sink_right) = classify_stereo_ports(&sink_port_refs);

    let sink_left = match sink_left {
        Some(id) => id,
        None => return Vec::new(),
    };
    let sink_right = sink_right.unwrap_or(sink_left);

    eprintln!(
        "  [route] Re-linking node={} directly to sink={}",
        node_id, sink_node_id
    );

    let link_fn = if linger { create_lingering_link } else { create_link };

    let mut links = Vec::new();
    match (app_left, app_right) {
        (Some(al), Some(ar)) => {
            if let Some(l) = link_fn(core, node_id, al, sink_node_id, sink_left) {
                links.push(l);
            }
            if let Some(l) = link_fn(core, node_id, ar, sink_node_id, sink_right) {
                links.push(l);
            }
        }
        (Some(al), None) => {
            if let Some(l) = link_fn(core, node_id, al, sink_node_id, sink_left) {
                links.push(l);
            }
            if let Some(l) = link_fn(core, node_id, al, sink_node_id, sink_right) {
                links.push(l);
            }
        }
        _ => {}
    }
    links
}

// ---------------------------------------------------------------------------
// Helper: destroy tracked links from a node to sink
// ---------------------------------------------------------------------------

fn destroy_links_to_sink(
    state: &mut PwState,
    registry: &pw::registry::RegistryRc,
    node_id: u32,
) {
    let sink_node_id = match state.sink_node_id {
        Some(id) => id,
        None => return,
    };

    let to_destroy: Vec<u32> = state
        .existing_links
        .iter()
        .filter(|(_, tl)| tl.output_node_id == node_id && tl.input_node_id == sink_node_id)
        .map(|(&id, _)| id)
        .collect();

    if !to_destroy.is_empty() {
        eprintln!(
            "  [route] Destroying {} existing links for node={} -> sink",
            to_destroy.len(),
            node_id
        );
        for link_id in &to_destroy {
            state.existing_links.remove(link_id);
            eprintln!("  [route]   destroy_global({})", link_id);
            let _ = registry.destroy_global(*link_id);
        }
    }
}

// ---------------------------------------------------------------------------
// PW thread entry point
// ---------------------------------------------------------------------------

pub fn run_pw_thread(
    config: Config,
    gui_tx: std::sync::mpsc::Sender<PwToGui>,
    sender_tx: std::sync::mpsc::Sender<pw::channel::Sender<GuiToPw>>,
    runtime_params: Arc<RuntimeParams>,
) {
    pw::init();

    let mainloop = MainLoopRc::new(None).expect("Failed to create PipeWire main loop");

    let (pw_sender, pw_receiver) = pw::channel::channel::<GuiToPw>();
    sender_tx
        .send(pw_sender)
        .expect("Failed to send PW sender to GUI thread");
    drop(sender_tx);

    let sample_rate = config.processing.sample_rate;

    // --- Create filter node ---

    let mut fg_state = Box::new(FilterAndGui {
        filter: FilterState::new(&config, runtime_params),
        gui_tx: gui_tx.clone(),
    });
    fg_state.filter.enable_debug_recording();

    let mut events: pw_sys::pw_filter_events = unsafe { std::mem::zeroed() };
    events.version = pw_sys::PW_VERSION_FILTER_EVENTS;
    events.process = Some(on_process);
    events.state_changed = Some(gui_on_state_changed);

    let node_props = properties! {
        "media.type" => "Audio",
        "media.category" => "Filter",
        "media.role" => "DSP",
        "node.name" => "SpectralBlend",
        "node.description" => "Spectral voice masking filter"
    };

    let filter_name = CString::new("SpectralBlend").unwrap();
    let state_ptr = &mut *fg_state as *mut FilterAndGui as *mut c_void;

    let filter = unsafe {
        pw_sys::pw_filter_new_simple(
            mainloop.loop_().as_raw_ptr(),
            filter_name.as_ptr(),
            node_props.into_raw(),
            &events as *const pw_sys::pw_filter_events,
            state_ptr,
        )
    };
    assert!(!filter.is_null(), "Failed to create PipeWire filter");

    unsafe {
        fg_state.filter.music_l_port =
            add_port(filter, pw::spa::sys::SPA_DIRECTION_INPUT, "music_left");
        fg_state.filter.music_r_port =
            add_port(filter, pw::spa::sys::SPA_DIRECTION_INPUT, "music_right");
        fg_state.filter.voice_l_port =
            add_port(filter, pw::spa::sys::SPA_DIRECTION_INPUT, "voice_left");
        fg_state.filter.voice_r_port =
            add_port(filter, pw::spa::sys::SPA_DIRECTION_INPUT, "voice_right");
        fg_state.filter.output_l_port =
            add_port(filter, pw::spa::sys::SPA_DIRECTION_OUTPUT, "output_left");
        fg_state.filter.output_r_port =
            add_port(filter, pw::spa::sys::SPA_DIRECTION_OUTPUT, "output_right");
    }

    let ret = unsafe {
        pw_sys::pw_filter_connect(
            filter,
            pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS,
            ptr::null_mut(),
            0,
        )
    };
    assert!(ret >= 0, "Failed to connect filter: error {ret}");

    // --- Core + Registry ---

    let context =
        pw::context::ContextRc::new(&mainloop, None).expect("Failed to create PW context");
    let core = context
        .connect_rc(None)
        .expect("Failed to connect to PipeWire");
    let registry = core
        .get_registry_rc()
        .expect("Failed to get PW registry");

    // Sequence number for shutdown sync (ensures lingering links are flushed)
    let shutdown_seq: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));

    let _core_listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            let shutdown_seq = shutdown_seq.clone();
            move |_id, seq| {
                let expected = shutdown_seq.borrow();
                if let Some(expected_seq) = *expected {
                    if seq.seq() == expected_seq {
                        eprintln!("  [shutdown] Sync done, quitting mainloop");
                        mainloop.quit();
                    }
                }
            }
        })
        .register();

    let state = Rc::new(RefCell::new(PwState {
        nodes: HashMap::new(),
        ports: HashMap::new(),
        filter_node_id: None,
        filter_input_ports: HashMap::new(),
        filter_output_ports: HashMap::new(),
        sink_node_id: None,
        sink_input_ports: HashMap::new(),
        active_links: HashMap::new(),
        restore_links: HashMap::new(),
        existing_links: HashMap::new(),
        routed_nodes: std::collections::HashSet::new(),
        output_links: Vec::new(),
    }));

    // --- Registry listener ---

    let _listener = registry
        .add_listener_local()
        .global({
            let state = state.clone();
            let gui_tx = gui_tx.clone();
            let registry = registry.clone();
            let core = core.clone();
            move |global| {
                let props: &pw::spa::utils::dict::DictRef = match global.props {
                    Some(p) => p,
                    None => return,
                };

                match global.type_ {
                    ObjectType::Node => {
                        let node_name = props.get("node.name").unwrap_or("");
                        let media_class = props.get("media.class").unwrap_or("");

                        if node_name == "SpectralBlend" {
                            eprintln!("  [registry] Filter node id={}", global.id);
                            let mut s = state.borrow_mut();
                            s.filter_node_id = Some(global.id);
                            try_link_filter_output(&mut s, &core);
                            let _ = gui_tx.send(PwToGui::FilterReady);
                            return;
                        }

                        if media_class == "Audio/Sink" && state.borrow().sink_node_id.is_none() {
                            eprintln!(
                                "  [registry] Sink node id={} name=\"{}\"",
                                global.id, node_name
                            );
                            let mut s = state.borrow_mut();
                            s.sink_node_id = Some(global.id);
                            try_link_filter_output(&mut s, &core);
                            return;
                        }

                        if media_class.contains("Stream")
                            && media_class.contains("Output")
                            && media_class.contains("Audio")
                        {
                            let display_name = props
                                .get("application.name")
                                .or_else(|| props.get("node.description"))
                                .unwrap_or(node_name)
                                .to_string();

                            eprintln!(
                                "  [registry] App node id={} name=\"{}\"",
                                global.id, display_name
                            );

                            state.borrow_mut().nodes.insert(
                                global.id,
                                NodeInfo {
                                    name: display_name.clone(),
                                },
                            );
                            let _ = gui_tx.send(PwToGui::NodeAdded {
                                node_id: global.id,
                                name: display_name,
                            });
                        }
                    }
                    ObjectType::Port => {
                        let node_id: u32 =
                            match props.get("node.id").and_then(|s: &str| s.parse().ok()) {
                                Some(id) => id,
                                None => return,
                            };
                        let port_name = props.get("port.name").unwrap_or("").to_string();
                        let direction = props.get("port.direction").unwrap_or("").to_string();

                        let mut s = state.borrow_mut();

                        if Some(node_id) == s.filter_node_id && direction == "in" {
                            eprintln!(
                                "  [registry] Filter input port id={} name=\"{}\"",
                                global.id, port_name
                            );
                            s.filter_input_ports.insert(port_name, global.id);
                            return;
                        }

                        if Some(node_id) == s.filter_node_id && direction == "out" {
                            eprintln!(
                                "  [registry] Filter output port id={} name=\"{}\"",
                                global.id, port_name
                            );
                            s.filter_output_ports.insert(port_name, global.id);
                            try_link_filter_output(&mut s, &core);
                            return;
                        }

                        if Some(node_id) == s.sink_node_id && direction == "in" {
                            eprintln!(
                                "  [registry] Sink input port id={} name=\"{}\"",
                                global.id, port_name
                            );
                            s.sink_input_ports.insert(global.id, port_name);
                            try_link_filter_output(&mut s, &core);
                            return;
                        }

                        if s.nodes.contains_key(&node_id) && direction == "out" {
                            eprintln!(
                                "  [registry] App output port id={} node={} name=\"{}\"",
                                global.id, node_id, port_name
                            );
                            s.ports.insert(
                                global.id,
                                PortInfo {
                                    node_id,
                                    name: port_name,
                                    direction,
                                },
                            );
                            let _ = gui_tx.send(PwToGui::PortAdded {
                                port_id: global.id,
                                node_id,
                            });
                        }
                    }
                    ObjectType::Link => {
                        let output_node: u32 = match props
                            .get("link.output.node")
                            .and_then(|s: &str| s.parse().ok())
                        {
                            Some(id) => id,
                            None => return,
                        };

                        let input_node: u32 = match props
                            .get("link.input.node")
                            .and_then(|s: &str| s.parse().ok())
                        {
                            Some(id) => id,
                            None => return,
                        };

                        let s = state.borrow();

                        if Some(input_node) == s.filter_node_id {
                            return;
                        }
                        if Some(output_node) == s.filter_node_id {
                            return;
                        }

                        if s.routed_nodes.contains(&output_node) {
                            drop(s);
                            eprintln!(
                                "  [link] Destroying competing link id={} from routed node={}",
                                global.id, output_node
                            );
                            let _ = registry.destroy_global(global.id);
                            return;
                        }

                        drop(s);

                        eprintln!(
                            "  [link] Tracking link id={} node={} -> node={}",
                            global.id, output_node, input_node
                        );
                        state.borrow_mut().existing_links.insert(
                            global.id,
                            TrackedLink {
                                output_node_id: output_node,
                                input_node_id: input_node,
                            },
                        );
                    }
                    _ => {}
                }
            }
        })
        .global_remove({
            let state = state.clone();
            let gui_tx = gui_tx.clone();
            move |id| {
                let mut s = state.borrow_mut();

                if s.nodes.remove(&id).is_some() {
                    s.active_links.remove(&id);
                    s.restore_links.remove(&id);
                    s.routed_nodes.remove(&id);
                    s.ports.retain(|_, p| p.node_id != id);
                    s.existing_links.retain(|_, tl| tl.output_node_id != id);
                    let _ = gui_tx.send(PwToGui::NodeRemoved { node_id: id });
                    return;
                }

                if s.ports.remove(&id).is_some() {
                    let _ = gui_tx.send(PwToGui::PortRemoved { port_id: id });
                }

                s.existing_links.remove(&id);
            }
        })
        .register();

    // --- GUI → PW channel receiver ---

    let _receiver = pw_receiver.attach(mainloop.loop_(), {
        let state = state.clone();
        let core = core.clone();
        let registry = registry.clone();
        let mainloop = mainloop.clone();

        move |msg| match msg {
            GuiToPw::SetRole { node_id, role } => {
                let mut s = state.borrow_mut();

                if role == AppRole::None {
                    // UN-ROUTING: create sink links FIRST (and store them!),
                    // then remove filter links. No gap in connectivity.
                    eprintln!("  [route] Un-routing node={}", node_id);
                    let sink_links = create_sink_links(&s, &core, node_id, false);
                    if !sink_links.is_empty() {
                        s.restore_links.insert(node_id, sink_links);
                    }
                    if let Some(links) = s.active_links.remove(&node_id) {
                        eprintln!(
                            "  [route] Removing {} filter links for node={}",
                            links.len(),
                            node_id
                        );
                        drop(links);
                    }
                    s.routed_nodes.remove(&node_id);
                    return;
                }

                // ROUTING: create filter links first, then destroy sink links.
                s.routed_nodes.insert(node_id);

                // Drop any restore links we were holding for this node
                s.restore_links.remove(&node_id);

                let role_name = match role {
                    AppRole::Music => "Music",
                    AppRole::Voice => "Voice",
                    AppRole::None => unreachable!(),
                };
                let (left_name, right_name) = match role {
                    AppRole::Music => ("music_left", "music_right"),
                    AppRole::Voice => ("voice_left", "voice_right"),
                    AppRole::None => unreachable!(),
                };

                let filter_node_id = match s.filter_node_id {
                    Some(id) => id,
                    None => {
                        eprintln!("  [route] Filter node not ready!");
                        return;
                    }
                };
                let left_port_id = match s.filter_input_ports.get(left_name) {
                    Some(&id) => id,
                    None => {
                        eprintln!("  [route] Filter port '{}' not found!", left_name);
                        return;
                    }
                };
                let right_port_id = match s.filter_input_ports.get(right_name) {
                    Some(&id) => id,
                    None => {
                        eprintln!("  [route] Filter port '{}' not found!", right_name);
                        return;
                    }
                };

                let app_ports: Vec<(u32, String)> = s
                    .ports
                    .iter()
                    .filter(|(_, p)| p.node_id == node_id && p.direction == "out")
                    .map(|(&id, p)| (id, p.name.clone()))
                    .collect();

                if app_ports.is_empty() {
                    eprintln!("  [route] No output ports for node={}!", node_id);
                    return;
                }

                let app_port_refs: Vec<(u32, &str)> =
                    app_ports.iter().map(|(id, n)| (*id, n.as_str())).collect();
                let (app_left, app_right) = classify_stereo_ports(&app_port_refs);

                let targets = match (app_left, app_right) {
                    (Some(al), Some(ar)) => vec![(al, left_port_id), (ar, right_port_id)],
                    (Some(al), None) => vec![(al, left_port_id), (al, right_port_id)],
                    _ => {
                        eprintln!("  [route] Could not classify ports for node={}!", node_id);
                        return;
                    }
                };

                eprintln!(
                    "  [route] Creating {} links: node={} -> filter ({}) ports {:?}",
                    targets.len(),
                    node_id,
                    role_name,
                    targets
                );

                let mut links = Vec::new();
                for (out_port, in_port) in targets {
                    if let Some(link) =
                        create_link(&core, node_id, out_port, filter_node_id, in_port)
                    {
                        links.push(link);
                    }
                }

                // Remove old filter links (when switching roles)
                if let Some(old_links) = s.active_links.remove(&node_id) {
                    eprintln!(
                        "  [route] Removing {} old filter links for node={}",
                        old_links.len(),
                        node_id
                    );
                    drop(old_links);
                }

                // Destroy existing links to sink
                destroy_links_to_sink(&mut s, &registry, node_id);

                if !links.is_empty() {
                    s.active_links.insert(node_id, links);
                }
            }
            GuiToPw::Quit => {
                let mut s = state.borrow_mut();
                let routed: Vec<u32> = s.routed_nodes.iter().copied().collect();
                for &node_id in &routed {
                    eprintln!("  [route] Shutdown: un-routing node={}", node_id);
                    // Use lingering links so they survive process exit.
                    // No need to store them — PipeWire keeps them alive independently.
                    create_sink_links(&s, &core, node_id, true);
                    if let Some(links) = s.active_links.remove(&node_id) {
                        drop(links);
                    }
                    s.routed_nodes.remove(&node_id);
                }
                s.output_links.clear();
                drop(s);
                // Sync with daemon — quit only after it confirms our
                // lingering link creations have been processed.
                match core.sync(0) {
                    Ok(seq) => {
                        eprintln!("  [shutdown] Waiting for sync seq={}", seq.seq());
                        *shutdown_seq.borrow_mut() = Some(seq.seq());
                    }
                    Err(_) => mainloop.quit(),
                }
            }
        }
    });

    // --- Run ---
    mainloop.run();

    // --- Write debug recordings ---
    if fg_state.filter.debug_recording {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };

        let write_stereo = |path: &str, left: &[f32], right: &[f32]| {
            if let Ok(mut w) = hound::WavWriter::create(path, spec) {
                for i in 0..left.len().min(right.len()) {
                    let _ = w.write_sample(left[i]);
                    let _ = w.write_sample(right[i]);
                }
                let _ = w.finalize();
                eprintln!("  [debug] Wrote {} ({} samples)", path, left.len());
            }
        };

        write_stereo(
            "debug_music_in.wav",
            &fg_state.filter.debug_music_l,
            &fg_state.filter.debug_music_r,
        );
        write_stereo(
            "debug_voice_in.wav",
            &fg_state.filter.debug_voice_l,
            &fg_state.filter.debug_voice_r,
        );
        write_stereo(
            "debug_output.wav",
            &fg_state.filter.debug_output_l,
            &fg_state.filter.debug_output_r,
        );
    }

    // --- Cleanup ---
    drop(_receiver);
    drop(_core_listener);
    drop(_listener);
    drop(state);

    unsafe { pw_sys::pw_filter_destroy(filter) };
    drop(fg_state);
}
