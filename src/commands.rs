use chrono::Utc;
use serde_json::{json, Value};

const VERSION: &str = "1.19.0";

fn make_headers(jid: &str, body_size: usize) -> Value {
    let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S%z").to_string();
    json!({
        "from": jid,
        "to": "master",
        "timestamp": timestamp,
        "method": "SET",
        "type": "data",
        "version": VERSION,
        "size": body_size
    })
}

fn make_message(jid: &str, body: Value) -> String {
    let body_str = body.to_string();
    let headers = make_headers(jid, body_str.len());
    let msg = json!({
        "headers": headers,
        "body": body
    });
    msg.to_string()
}

/// Build a SET command for a node (room, actuator, fan).
/// `fields` is a JSON object with the fields to set, e.g. {"setpoint": 21.0}
pub fn set_node(jid: &str, node_id: u16, fields: Value) -> String {
    let body = json!({
        "nodes": [{
            "id": node_id,
            "data": fields
        }]
    });
    make_message(jid, body)
}

/// Build a SET command for holiday mode on/off.
pub fn set_holiday_mode(jid: &str, on: bool) -> String {
    let body = json!({
        "holiday_mode": {
            "onoff": if on { "on" } else { "off" }
        }
    });
    make_message(jid, body)
}

/// Build a SET command for freecooling on/off.
pub fn set_freecooling(jid: &str, on: bool) -> String {
    let body = json!({
        "freecooling": {
            "onoff": if on { "on" } else { "off" }
        }
    });
    make_message(jid, body)
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
                if let Ok(temp) = payload.trim().parse::<f32>() {
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
                let on = match mode.as_str() {
                    "heat" | "on" => true,
                    "off" => false,
                    _ => return None,
                };
                return Some(ESmartCommand {
                    xmpp_payload: set_node(
                        jid,
                        node_id,
                        json!({"deviceOnOff": if on { "on" } else { "off" }}),
                    ),
                    description: format!(
                        "Set room {} heating to {}",
                        node_id,
                        if on { "on" } else { "off" }
                    ),
                });
            }
        }
    }

    // Actuator cover commands: actuator_<id>_position, actuator_<id>_tilt
    if let Some(rest) = entity_id.strip_prefix("actuator_") {
        if let Some(id_str) = rest.strip_suffix("_position") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                if let Ok(pos) = payload.trim().parse::<f32>() {
                    let pos = pos.clamp(0.0, 100.0);
                    return Some(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"position": pos})),
                        description: format!("Set actuator {} position to {}%", node_id, pos),
                    });
                }
            }
        }
        if let Some(id_str) = rest.strip_suffix("_tilt") {
            if let Ok(node_id) = id_str.parse::<u16>() {
                if let Ok(tilt) = payload.trim().parse::<f32>() {
                    let tilt = tilt.clamp(0.0, 100.0);
                    return Some(ESmartCommand {
                        xmpp_payload: set_node(jid, node_id, json!({"orientation": tilt})),
                        description: format!(
                            "Set actuator {} orientation to {}%",
                            node_id, tilt
                        ),
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
