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
    /// When the command was created, so stale ones can be dropped.
    pub created: Instant,
}

/// Result of parsing an MQTT command.
#[derive(Debug)]
pub enum ParseResult {
    /// An XMPP command to forward to the eSmart device.
    Command(ESmartCommand),
    /// A travel time was set locally (no XMPP needed); the caller persists it
    /// and echoes it back to the state topic.
    TravelTime { node_id: u16, dir: &'static str, secs: f32 },
    /// The topic/payload was not recognized.
    Unrecognized,
}

/// Fan speeds the eSmart accepts. Anything else is answered with
/// "invalid values" (verified on a 3-speed ventilation unit).
pub const FAN_SPEEDS: [&str; 4] = ["0%", "33%", "66%", "99%"];

/// Map a 0-100 percentage to the closest accepted fan speed.
pub fn snap_fan_speed(pct: f32) -> &'static str {
    let pct = pct.clamp(0.0, 100.0);
    if pct <= 0.0 {
        FAN_SPEEDS[0]
    } else if pct <= 33.0 {
        FAN_SPEEDS[1]
    } else if pct <= 66.0 {
        FAN_SPEEDS[2]
    } else {
        FAN_SPEEDS[3]
    }
}

/// Last non-zero speed reported per fan, so ON restores it.
pub type FanStates = Arc<Mutex<HashMap<u16, String>>>;

pub fn new_fan_states() -> FanStates {
    Arc::new(Mutex::new(HashMap::new()))
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
    /// Estimated HA position from the last STOP command, used to handle
    /// HA sending STOP to both position and tilt topics (double STOP).
    pub last_stop_position: Option<f32>,
}

impl ActuatorState {
    pub fn new() -> Self {
        Self {
            ha_position: 0.0,
            ha_orientation: 0.0,
            travel_time_open: 30.0,
            travel_time_close: 30.0,
            in_flight: None,
            last_stop_position: None,
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
    fan_states: &FanStates,
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
                    return ParseResult::TravelTime { node_id, dir: "open", secs };
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
                    return ParseResult::TravelTime { node_id, dir: "close", secs };
                }
            }
        }
    }

    // Raw passthrough, for trying out eSmart features the bridge does not know yet.
    // Payload is either a node body ({"id": 337, "speed": "33%"}), sent as an
    // "operation", or {"type": "<header type>", "body": {...}}.
    if entity_id == "raw_command" {
        let value: Value = match serde_json::from_str(payload.trim()) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("raw_command payload is not valid JSON: {}", e);
                return ParseResult::Unrecognized;
            }
        };
        let (msg_type, body) = match (value.get("type").and_then(|t| t.as_str()), value.get("body")) {
            (Some(t), Some(b)) => (t.to_string(), b.clone()),
            _ => ("operation".to_string(), value),
        };
        if !body.is_object() {
            log::warn!("raw_command body must be a JSON object");
            return ParseResult::Unrecognized;
        }
        return ParseResult::Command(ESmartCommand {
            created: Instant::now(),
            xmpp_payload: make_message(jid, &msg_type, body.clone()),
            description: format!("Raw {} command {}", msg_type, body),
        });
    }

    // Holiday mode switch
    if entity_id == "holiday_mode" {
        let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
        return ParseResult::Command(ESmartCommand {
            created: Instant::now(),
            xmpp_payload: set_holiday_mode(jid, on),
            description: format!("Set holiday mode to {}", if on { "on" } else { "off" }),
        });
    }

    // Freecooling switch
    if entity_id == "freecooling" {
        let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
        return ParseResult::Command(ESmartCommand {
            created: Instant::now(),
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
                        created: Instant::now(),
                        xmpp_payload: set_node(jid, node_id, json!({"setpoint": "MAX"})),
                        description: format!("Set room {} setpoint to MAX", node_id),
                    });
                }
                if let Ok(temp) = trimmed.parse::<f32>() {
                    // Device only accepts integer setpoints in range 18-24
                    let setpoint = temp.round().clamp(18.0, 24.0) as i32;
                    return ParseResult::Command(ESmartCommand {
                        created: Instant::now(),
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
                            state.last_stop_position = None;
                        }
                        // eSmart 0 = fully open
                        return ParseResult::Command(ESmartCommand {
                            created: Instant::now(),
                            xmpp_payload: set_node(jid, node_id, json!({"position": 0, "orientation": 0})),
                            description: format!("Open actuator {} (eSmart position 0)", node_id),
                        });
                    }
                    "CLOSE" => {
                        // Record in-flight move for STOP estimation
                        if let Ok(mut states) = actuator_states.lock() {
                            let state = states.entry(node_id).or_insert_with(ActuatorState::new);
                            state.in_flight = Some((Instant::now(), state.ha_position, 0.0));
                            state.last_stop_position = None;
                        }
                        // eSmart 1024 = fully closed
                        return ParseResult::Command(ESmartCommand {
                            created: Instant::now(),
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
                            state.in_flight = None;
                            state.last_stop_position = Some(pos);
                            (pos, orient)
                        } else {
                            (50.0, 512) // Fallback to mid-position
                        };
                        let es_pos = ((100.0 - estimated_ha.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32;
                        log::info!("STOP actuator {}: estimated HA pos {:.1}%, eSmart pos {}, orientation {}", node_id, estimated_ha, es_pos, es_orient);
                        return ParseResult::Command(ESmartCommand {
                            created: Instant::now(),
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
                                state.last_stop_position = None;
                            }
                            // Invert and scale: HA 100=open → eSmart 0=open
                            let es_pos = ((100.0 - ha_clamped) / 100.0 * 1024.0).round() as i32;
                            return ParseResult::Command(ESmartCommand {
                                created: Instant::now(),
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
                // HA may send STOP to tilt topic instead of (or in addition to) position topic.
                // Perform the same stop logic: estimate position, send both position+orientation.
                if payload.trim().eq_ignore_ascii_case("STOP") {
                    let (estimated_ha, es_orient) = if let Ok(mut states) = actuator_states.lock() {
                        let state = states.entry(node_id).or_insert_with(ActuatorState::new);
                        // If position STOP already fired and cleared in_flight,
                        // reuse its estimate instead of falling back to ha_position (target).
                        let pos = if state.in_flight.is_some() {
                            state.estimate_position()
                        } else if let Some(last) = state.last_stop_position {
                            last
                        } else {
                            state.ha_position
                        };
                        let orient = ((100.0 - state.ha_orientation.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32;
                        state.in_flight = None;
                        state.last_stop_position = Some(pos);
                        (pos, orient)
                    } else {
                        (50.0, 512)
                    };
                    let es_pos = ((100.0 - estimated_ha.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32;
                    log::info!("STOP actuator {} (via tilt): estimated HA pos {:.1}%, eSmart pos {}, orientation {}", node_id, estimated_ha, es_pos, es_orient);
                    return ParseResult::Command(ESmartCommand {
                        created: Instant::now(),
                        xmpp_payload: set_node(jid, node_id, json!({"position": es_pos, "orientation": es_orient})),
                        description: format!("Stop actuator {} via tilt (estimated HA {:.0}% → eSmart pos {} orient {})", node_id, estimated_ha, es_pos, es_orient),
                    });
                }
                if let Ok(ha_tilt) = payload.trim().parse::<f32>() {
                    let ha_clamped = ha_tilt.clamp(0.0, 100.0);
                    let es_orient = ((100.0 - ha_clamped) / 100.0 * 1024.0).round() as i32;
                    // Device requires both position and orientation together.
                    // Use estimated position (not ha_position) because the device
                    // reports the target position immediately while still moving.
                    let es_pos = if let Ok(states) = actuator_states.lock() {
                        let state = states.get(&node_id);
                        let ha_pos = state.map(|s| s.estimate_position()).unwrap_or(0.0);
                        ((100.0 - ha_pos.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32
                    } else {
                        512
                    };
                    return ParseResult::Command(ESmartCommand {
                        created: Instant::now(),
                        xmpp_payload: set_node(jid, node_id, json!({"position": es_pos, "orientation": es_orient})),
                        description: format!("Set actuator {} tilt to eSmart orientation {} (keeping pos {})", node_id, es_orient, es_pos),
                    });
                }
            }
        }
    }

    // Fan commands: fan_<id>_speed (0-100 %), fan_<id>_level (0-3), fan_<id>_mode, fan_<id>_onoff
    if let Some(rest) = entity_id.strip_prefix("fan_") {
        let fan_cmd = |node_id: u16, speed: &str, why: String| {
            ParseResult::Command(ESmartCommand {
                created: Instant::now(),
                xmpp_payload: set_node(jid, node_id, json!({"speed": speed, "mode": "manual"})),
                description: format!("Set fan {} speed to {} ({})", node_id, speed, why),
            })
        };
        if let Some(id_str) = rest.strip_suffix("_speed") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                // Accept "50", "50%" or "50.0"; snap to a speed the device accepts
                let raw = payload.trim().trim_end_matches('%');
                if let Ok(pct) = raw.parse::<f32>() {
                    return fan_cmd(node_id, snap_fan_speed(pct), format!("requested {}%", pct));
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_level") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                if let Ok(level) = payload.trim().parse::<f32>() {
                    let idx = (level.round() as i64).clamp(0, 3) as usize;
                    return fan_cmd(node_id, FAN_SPEEDS[idx], format!("level {}", idx));
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_mode") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let mode = payload.trim().to_lowercase();
                if mode == "auto" || mode == "manual" {
                    return ParseResult::Command(ESmartCommand {
                        created: Instant::now(),
                        xmpp_payload: set_node(jid, node_id, json!({"mode": mode})),
                        description: format!("Set fan {} mode to {}", node_id, mode),
                    });
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_onoff") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
                // ON restores the last non-zero speed seen, else the lowest speed
                let speed = if on {
                    fan_states
                        .lock()
                        .ok()
                        .and_then(|f| f.get(&node_id).cloned())
                        .unwrap_or_else(|| FAN_SPEEDS[1].to_string())
                } else {
                    FAN_SPEEDS[0].to_string()
                };
                return fan_cmd(node_id, &speed, if on { "on".into() } else { "off".into() });
            }
        }
    }

    log::warn!("Unrecognized command topic: {} payload: {}", topic, payload);
    ParseResult::Unrecognized
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(r: ParseResult) -> Value {
        match r {
            ParseResult::Command(c) => serde_json::from_str::<Value>(&c.xmpp_payload).unwrap()["body"].clone(),
            other => panic!("expected command, got {:?}", other),
        }
    }

    #[test]
    fn fan_speed_snaps_to_accepted_values() {
        assert_eq!(snap_fan_speed(0.0), "0%");
        assert_eq!(snap_fan_speed(10.0), "33%");
        assert_eq!(snap_fan_speed(50.0), "66%");
        assert_eq!(snap_fan_speed(100.0), "99%");
        let (a, f) = (new_actuator_states(), new_fan_states());
        assert_eq!(body(parse_mqtt_command("j", "esmart/fan_337_speed/set", "100%", &a, &f))["speed"], "99%");
        assert_eq!(body(parse_mqtt_command("j", "esmart/fan_337_level/set", "2", &a, &f))["speed"], "66%");
    }

    #[test]
    fn fan_on_restores_last_speed() {
        let (a, f) = (new_actuator_states(), new_fan_states());
        assert_eq!(body(parse_mqtt_command("j", "esmart/fan_337_onoff/set", "ON", &a, &f))["speed"], "33%");
        f.lock().unwrap().insert(337, "66%".into());
        assert_eq!(body(parse_mqtt_command("j", "esmart/fan_337_onoff/set", "ON", &a, &f))["speed"], "66%");
        assert_eq!(body(parse_mqtt_command("j", "esmart/fan_337_onoff/set", "OFF", &a, &f))["speed"], "0%");
    }

    #[test]
    fn raw_command_passthrough() {
        let (a, f) = (new_actuator_states(), new_fan_states());
        let b = body(parse_mqtt_command("j", "esmart/raw_command/set", r#"{"id":5,"x":1}"#, &a, &f));
        assert_eq!(b["id"], 5);
        assert!(matches!(
            parse_mqtt_command("j", "esmart/raw_command/set", "not json", &a, &f),
            ParseResult::Unrecognized
        ));
    }
}
