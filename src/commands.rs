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

/// Build a CMD/operation for a blind stop/step command.
/// Body: {"id": N, "blind_operation": "stop"/"step_up"/"step_down"}
pub fn blind_operation(jid: &str, node_id: u16, operation: &str) -> String {
    let body = json!({
        "id": node_id,
        "blind_operation": operation
    });
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

/// Parse an MQTT command topic and payload into an ESmartCommand.
/// Topic format: esmart/<entity_id>/set
/// Returns None if the topic/payload is not recognized.
pub fn parse_mqtt_command(jid: &str, topic: &str, payload: &str) -> Option<ESmartCommand> {
    let parts: Vec<&str> = topic.split('/').collect();
    // Expected: ["esmart", "<entity_id>", "set"]
    if parts.len() != 3 || parts[0] != "esmart" || parts[2] != "set" {
        return None;
    }
    let entity_id = parts[1];

    // Holiday mode switch
    if entity_id == "holiday_mode" {
        let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
        return Some(ESmartCommand {
            xmpp_payload: set_holiday_mode(jid, on),
            description: format!("Set holiday mode to {}", if on { "on" } else { "off" }),
        });
    }

    // Freecooling switch
    if entity_id == "freecooling" {
        let on = matches!(payload.trim().to_uppercase().as_str(), "ON" | "1" | "TRUE");
        return Some(ESmartCommand {
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
                    return Some(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"setpoint": "MAX"})),
                        description: format!("Set room {} setpoint to MAX", node_id),
                    });
                }
                if let Ok(temp) = trimmed.parse::<f32>() {
                    return Some(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"setpoint": temp})),
                        description: format!("Set room {} setpoint to {}°C", node_id, temp),
                    });
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_mode") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let mode = payload.trim().to_lowercase();
                // eSmart doesn't accept direct onoff commands for rooms.
                // The device auto-manages on/off based on setpoint vs temperature.
                // "off" → set setpoint to minimum (5°C) to effectively disable heating.
                // "heat" → set setpoint to a reasonable default (21°C) to enable heating.
                let (setpoint, desc) = match mode.as_str() {
                    "heat" | "on" => (21.0, "on (setpoint 21°C)"),
                    "off" => (5.0, "off (setpoint 5°C)"),
                    _ => return None,
                };
                return Some(ESmartCommand {
                    xmpp_payload: set_node(
                        jid,
                        node_id,
                        json!({"setpoint": setpoint}),
                    ),
                    description: format!(
                        "Set room {} heating to {}",
                        node_id, desc
                    ),
                });
            }
        }
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
                        // eSmart 0 = fully open
                        return Some(ESmartCommand {
                            xmpp_payload: set_node(jid, node_id, json!({"position": 0, "orientation": 0})),
                            description: format!("Open actuator {} (eSmart position 0)", node_id),
                        });
                    }
                    "CLOSE" => {
                        // eSmart 1024 = fully closed
                        return Some(ESmartCommand {
                            xmpp_payload: set_node(jid, node_id, json!({"position": 1024, "orientation": 1024})),
                            description: format!("Close actuator {} (eSmart position 1024)", node_id),
                        });
                    }
                    "STOP" => {
                        return Some(ESmartCommand {
                            xmpp_payload: blind_operation(jid, node_id, "stop"),
                            description: format!("Stop actuator {}", node_id),
                        });
                    }
                    _ => {
                        if let Ok(ha_pos) = trimmed.parse::<f32>() {
                            // Invert and scale: HA 100=open → eSmart 0=open
                            let es_pos = ((100.0 - ha_pos.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32;
                            return Some(ESmartCommand {
                                xmpp_payload: set_node(jid, node_id, json!({"position": es_pos, "orientation": es_pos})),
                                description: format!("Set actuator {} position to eSmart {}", node_id, es_pos),
                            });
                        }
                    }
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_tilt") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let trimmed = payload.trim();
                match trimmed.to_uppercase().as_str() {
                    "OPEN" => {
                        return Some(ESmartCommand {
                            xmpp_payload: set_node(jid, node_id, json!({"orientation": 0})),
                            description: format!("Open actuator {} tilt (eSmart orientation 0)", node_id),
                        });
                    }
                    "CLOSE" => {
                        return Some(ESmartCommand {
                            xmpp_payload: set_node(jid, node_id, json!({"orientation": 1024})),
                            description: format!("Close actuator {} tilt (eSmart orientation 1024)", node_id),
                        });
                    }
                    "STOP" => {
                        return Some(ESmartCommand {
                            xmpp_payload: blind_operation(jid, node_id, "stop"),
                            description: format!("Stop actuator {} tilt", node_id),
                        });
                    }
                    _ => {
                        if let Ok(ha_tilt) = trimmed.parse::<f32>() {
                            // Invert and scale: HA 100=open → eSmart 0=open
                            let es_tilt = ((100.0 - ha_tilt.clamp(0.0, 100.0)) / 100.0 * 1024.0).round() as i32;
                            return Some(ESmartCommand {
                                xmpp_payload: set_node(jid, node_id, json!({"orientation": es_tilt})),
                                description: format!(
                                    "Set actuator {} orientation to eSmart {}",
                                    node_id, es_tilt
                                ),
                            });
                        }
                    }
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
                    return None;
                };
                return Some(ESmartCommand {
                    xmpp_payload: set_node(jid, node_id, json!({"speed": speed})),
                    description: format!("Set fan {} speed to {}", node_id, speed),
                });
            }
        }
        if let Some(id_str) = rest.strip_suffix("_mode") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                let mode = payload.trim().to_lowercase();
                if mode == "auto" || mode == "manual" {
                    return Some(ESmartCommand {
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
                return Some(ESmartCommand {
                    xmpp_payload: set_node(jid, node_id, json!({"speed": speed})),
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
    None
}
