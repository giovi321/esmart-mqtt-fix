use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Utc;
use serde_json::{json, Value};

const VERSION: &str = "1.19.0";

/// Build headers for a CMD message (matching the real eSmart app protocol).
/// `msg_type` is "operation" for node commands, "tablet_operation" for holiday/freecooling.
fn make_headers(jid: &str, msg_type: &str, body_size: usize) -> Value {
    let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%SZ").to_string();
    json!({
        "from": jid,
        "to": "master",
        "timestamp": timestamp,
        "method": "CMD",
        "type": msg_type,
        "version": VERSION,
        "size": body_size
    })
}

fn make_message(jid: &str, msg_type: &str, body: Value) -> String {
    let body_str = body.to_string();
    let headers = make_headers(jid, msg_type, body_str.len());
    let msg = json!({
        "headers": headers,
        "body": body
    });
    msg.to_string()
}

/// Build a CMD/operation for a node (room, actuator, fan).
/// Body is a flat object: {"id": N, "field": value, ...}
pub fn set_node(jid: &str, node_id: u16, fields: Value) -> String {
    let mut body = json!({ "id": node_id });
    if let Value::Object(map) = fields {
        if let Value::Object(ref mut body_map) = body {
            for (k, v) in map {
                body_map.insert(k, v);
            }
        }
    }
    make_message(jid, "operation", body)
}

/// Build a CMD/tablet_operation for holiday mode on/off.
/// Body: {"type": "holiday_mode", "onoff": "on"/"off"}
pub fn set_holiday_mode(jid: &str, on: bool) -> String {
    let body = json!({
        "type": "holiday_mode",
        "onoff": if on { "on" } else { "off" }
    });
    make_message(jid, "tablet_operation", body)
}

/// Build a CMD/tablet_operation for freecooling on/off.
/// Body: {"type": "regulated_freecooling", "onoff": "on"/"off"}
pub fn set_freecooling(jid: &str, on: bool) -> String {
    let body = json!({
        "type": "regulated_freecooling",
        "onoff": if on { "on" } else { "off" }
    });
    make_message(jid, "tablet_operation", body)
}

/// Represents a command to send to the eSmart device via XMPP.
#[derive(Debug, Clone)]
pub struct ESmartCommand {
    pub xmpp_payload: String,
    pub description: String,
}

/// Result of parsing an MQTT command.
#[derive(Debug)]
pub enum ParseResult {
    /// An XMPP command to forward to the eSmart device.
    Command(ESmartCommand),
    /// The command was handled locally (e.g. travel time config), no XMPP needed.
    Handled,
    /// The topic/payload was not recognized.
    Unrecognized,
}

/// Tracks the state of a single actuator for STOP position estimation.
#[derive(Debug, Clone)]
pub struct ActuatorState {
    /// Last known HA position (0=closed, 100=open) from device reports.
    pub ha_position: f32,
    /// Last known HA orientation/tilt (0=shade, 100=light) from device reports.
    pub ha_orientation: f32,
    /// Travel time in seconds for fully opening (0→100 in HA).
    pub travel_time_open: f32,
    /// Travel time in seconds for fully closing (100→0 in HA).
    pub travel_time_close: f32,
    /// In-flight move info: (start_instant, start_ha_pos, target_ha_pos).
    pub in_flight: Option<(Instant, f32, f32)>,
}

impl ActuatorState {
    pub fn new() -> Self {
        Self {
            ha_position: 0.0,
            ha_orientation: 0.0,
            travel_time_open: 30.0,
            travel_time_close: 30.0,
            in_flight: None,
        }
    }

    /// Estimate the current HA position based on elapsed time since move started.
    pub fn estimate_position(&self) -> f32 {
        match self.in_flight {
            Some((start, start_pos, target_pos)) => {
                let elapsed = start.elapsed().as_secs_f32();
                let distance = (target_pos - start_pos).abs();
                if distance < 0.01 {
                    return start_pos;
                }
                // Pick travel time based on direction
                let travel_time = if target_pos > start_pos {
                    self.travel_time_open
                } else {
                    self.travel_time_close
                };
                // Time for full 0-100 travel; scale by actual distance
                let time_for_distance = travel_time * distance / 100.0;
                let progress = (elapsed / time_for_distance).clamp(0.0, 1.0);
                let estimated = start_pos + (target_pos - start_pos) * progress;
                estimated.clamp(0.0, 100.0)
            }
            None => self.ha_position,
        }
    }
}

/// Shared state for all actuators.
pub type ActuatorStates = Arc<Mutex<HashMap<u16, ActuatorState>>>;

pub fn new_actuator_states() -> ActuatorStates {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Parse an MQTT command topic and payload into an ESmartCommand.
/// Topic format: esmart/<entity_id>/set
/// Returns None if the topic/payload is not recognized.
pub fn parse_mqtt_command(
    jid: &str,
    topic: &str,
    payload: &str,
    actuator_states: &ActuatorStates,
) -> ParseResult {
    let parts: Vec<&str> = topic.split('/').collect();
    // Expected: ["esmart", "<entity_id>", "set"]
    if parts.len() != 3 || parts[0] != "esmart" || parts[2] != "set" {
        return ParseResult::Unrecognized;
    }
    let entity_id = parts[1];

    // Travel time configuration: actuator_<id>_travel_open, actuator_<id>_travel_close
    if let Some(rest) = entity_id.strip_prefix("actuator_") {
        if let Some(id_str) = rest.strip_suffix("_travel_open") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                if let Ok(secs) = payload.trim().parse::<f32>() {
                    let secs = secs.clamp(1.0, 120.0);
                    if let Ok(mut states) = actuator_states.lock() {
                        states.entry(node_id).or_insert_with(ActuatorState::new).travel_time_open = secs;
                    }
                    log::info!("Set actuator {} open travel time to {}s", node_id, secs);
                    return ParseResult::Handled;
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_travel_close") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                if let Ok(secs) = payload.trim().parse::<f32>() {
                    let secs = secs.clamp(1.0, 120.0);
                    if let Ok(mut states) = actuator_states.lock() {
                        states.entry(node_id).or_insert_with(ActuatorState::new).travel_time_close = secs;
                    }
                    log::info!("Set actuator {} close travel time to {}s", node_id, secs);
                    return ParseResult::Handled;
                }
            }
        }
    }

    // Holiday mode switch
    if entity_id == "holiday_mode" {
        let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
        return ParseResult::Command(ESmartCommand {
            xmpp_payload: set_holiday_mode(jid, on),
            description: format!("Set holiday mode to {}", if on { "on" } else { "off" }),
        });
    }

    // Freecooling switch
    if entity_id == "freecooling" {
        let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
        return ParseResult::Command(ESmartCommand {
            xmpp_payload: set_freecooling(jid, on),
            description: format!("Set freecooling to {}", if on { "on" } else { "off" }),
        });
    }

    // Room climate commands: room_<id>_setpoint, room_<id>_mode
    if let Some(rest) = entity_id.strip_prefix("room_") {
        if let Some(id_str) = rest.strip_suffix("_setpoint") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let trimmed = payload.trim();
                // The real app sends "MAX" for the maximum setpoint
                if trimmed.eq_ignore_ascii_case("MAX") {
                    return ParseResult::Command(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"setpoint": "MAX"})),
                        description: format!("Set room {} setpoint to MAX", node_id),
                    });
                }
                if let Ok(temp) = trimmed.parse::<f32>() {
                    // Device only accepts integer setpoints in range 18-24
                    let setpoint = temp.round().clamp(18.0, 24.0) as i32;
                    return ParseResult::Command(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"setpoint": setpoint})),
                        description: format!("Set room {} setpoint to {}°C", node_id, setpoint),
                    });
                }
            }
        }
        // Mode is read-only in HA (no mode_command_topic); device auto-manages on/off.
    }

    // Actuator cover commands: actuator_<id>_position, actuator_<id>_tilt
    // HA uses 0-100 (100=open, 0=closed); eSmart uses 0-1024 (0=open, 1024=closed).
    // Conversion: esmart = ((100 - ha) / 100 * 1024).round()
    if let Some(rest) = entity_id.strip_prefix("actuator_") {
        if let Some(id_str) = rest.strip_suffix("_position") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let trimmed = payload.trim();
                match trimmed.to_uppercase().as_str() {
                    "OPEN" => {
                        // Record in-flight move for STOP estimation
                        if let Ok(mut states) = actuator_states.lock() {
                            let state = states.entry(node_id).or_insert_with(ActuatorState::new);
                            state.in_flight = Some((Instant::now(), state.ha_position, 100.0));
                        }
                        // eSmart 0 = fully open
                        return ParseResult::Command(ESmartCommand {
                            xmpp_payload: set_node(jid, node_id, json!({"position": 0, "orientation": 0})),
                            description: format!("Open actuator {} (eSmart position 0)", node_id),
                        });
                    }
                    "CLOSE" => {
                        // Record in-flight move for STOP estimation
                        if let Ok(mut states) = actuator_states.lock() {
                            let state = states.entry(node_id).or_insert_with(ActuatorState::new);
                            state.in_flight = Some((Instant::now(), state.ha_position, 0.0));
                        }
                        // eSmart 1024 = fully closed
                        return ParseResult::Command(ESmartCommand {
                            xmpp_payload: set_node(jid, node_id, json!({"position": 1024, "orientation": 1024})),
                            description: format!("Close actuator {} (eSmart position 1024)", node_id),
                        });
                    }
                    "STOP" => {
                        // Estimate current position from elapsed time and travel time,
                        // then send that position to freeze the blind in place.
                        // Device requires both position and orientation together.
                        let (estimated_ha, es_orient) = if let Ok(mut states) = actuator_states.lock() {
                            let state = states.entry(node_id).or_insert_with(ActuatorState::new);
                            let pos = state.estimate_position();
                            let orient = ((100.0 - state.ha_orientation.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32;
                            state.in_flight = None; // Clear in-flight
                            (pos, orient)
                        } else {
                            (50.0, 512) // Fallback to mid-position
                        };
                        let es_pos = ((100.0 - estimated_ha.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32;
                        log::info!("STOP actuator {}: estimated HA pos {:.1}%, eSmart pos {}, orientation {}", node_id, estimated_ha, es_pos, es_orient);
                        return ParseResult::Command(ESmartCommand {
                            xmpp_payload: set_node(jid, node_id, json!({"position": es_pos, "orientation": es_orient})),
                            description: format!("Stop actuator {} (estimated HA {:.0}% → eSmart pos {} orient {})", node_id, estimated_ha, es_pos, es_orient),
                        });
                    }
                    _ => {
                        if let Ok(ha_pos) = trimmed.parse::<f32>() {
                            let ha_clamped = ha_pos.clamp(0.0, 100.0);
                            // Record in-flight move for STOP estimation
                            if let Ok(mut states) = actuator_states.lock() {
                                let state = states.entry(node_id).or_insert_with(ActuatorState::new);
                                state.in_flight = Some((Instant::now(), state.ha_position, ha_clamped));
                            }
                            // Invert and scale: HA 100=open → eSmart 0=open
                            let es_pos = ((100.0 - ha_clamped) / 100.0 * 1024.0).round() as i32;
                            return ParseResult::Command(ESmartCommand {
                                xmpp_payload: set_node(jid, node_id, json!({"position": es_pos, "orientation": es_pos})),
                                description: format!("Set actuator {} position to eSmart {}", node_id, es_pos),
                            });
                        }
                    }
                }
            }
        }
        // Tilt command: actuator_<id>_tilt
        // HA tilt: 0=closed (shade), 100=open (light)
        // eSmart orientation: 0=light passing, 1024=complete shade
        // Conversion: esmart = round((100 - ha) / 100 * 1024)
        if let Some(id_str) = rest.strip_suffix("_tilt") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                // HA sends STOP to tilt topic too; ignore it (position STOP handles stopping)
                if payload.trim().eq_ignore_ascii_case("STOP") {
                    return ParseResult::Handled;
                }
                if let Ok(ha_tilt) = payload.trim().parse::<f32>() {
                    let ha_clamped = ha_tilt.clamp(0.0, 100.0);
                    let es_orient = ((100.0 - ha_clamped) / 100.0 * 1024.0).round() as i32;
                    return ParseResult::Command(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"orientation": es_orient})),
                        description: format!("Set actuator {} tilt to eSmart orientation {}", node_id, es_orient),
                    });
                }
            }
        }
    }

    // Fan commands: fan_<id>_speed, fan_<id>_mode, fan_<id>_onoff
    if let Some(rest) = entity_id.strip_prefix("fan_") {
        if let Some(id_str) = rest.strip_suffix("_speed") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let speed_str = payload.trim();
                // Accept "50", "50%" or raw percentage
                let speed = if speed_str.ends_with('%') {
                    speed_str.to_string()
                } else if let Ok(pct) = speed_str.parse::<u8>() {
                    format!("{}%", pct.min(100))
                } else {
                    return ParseResult::Unrecognized;
                };
                return ParseResult::Command(ESmartCommand {
                    xmpp_payload: set_node(jid, node_id, json!({"speed": speed, "mode": "manual"})),
                    description: format!("Set fan {} speed to {}", node_id, speed),
                });
            }
        }
        if let Some(id_str) = rest.strip_suffix("_mode") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let mode = payload.trim().to_lowercase();
                if mode == "auto" || mode == "manual" {
                    return ParseResult::Command(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"mode": mode})),
                        description: format!("Set fan {} mode to {}", node_id, mode),
                    });
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_onoff") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
                // Turn fan on/off by setting speed; ON defaults to 50% since we don't track previous speed
                let speed = if on { "50%" } else { "0%" };
                return ParseResult::Command(ESmartCommand {
                    xmpp_payload: set_node(jid, node_id, json!({"speed": speed, "mode": "manual"})),
                    description: format!(
                        "Set fan {} to {} (speed {})",
                        node_id,
                        if on { "on" } else { "off" },
                        speed
                    ),
                });
            }
        }
    }

    log::warn!("Unrecognized command topic: {} payload: {}", topic, payload);
    ParseResult::Unrecognized
}
