use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clap::Parser;
use data::{Meter, Node};
use rumqttc::{AsyncClient, ClientError, LastWill, MqttOptions};
use serde_json::json;
use stats::{DeviceClass, IterStats, Stat, StatValue, Unit};
use tokio::sync::{
    broadcast::{self, Receiver, Sender},
    mpsc,
};
use tokio_stream::StreamExt;
use tokio_xmpp::{Client, Error, Event};
use xmpp_parsers::{
    message::{Message, MessageType},
    muc::Muc,
    presence::{Presence, Show as PresenceShow, Type as PresenceType},
};

mod cli;
mod commands;
mod data;
mod stats;
mod totals;

type ChannelMessage = (String, Stat, StatValue);

/// Bridge availability, driven by MQTT connection (LWT) and by eSmart data freshness.
const AVAILABILITY_TOPIC: &str = "esmart/status";
/// Home Assistant announces "online" here when it (re)starts.
const HA_STATUS_TOPIC: &str = "homeassistant/status";

/// Reconnect delays grow from MIN to MAX, doubling on each consecutive failure.
const BACKOFF_MIN_SECS: u64 = 5;
const BACKOFF_MAX_SECS: u64 = 300;

fn next_backoff(current: u64) -> u64 {
    (current * 2).clamp(BACKOFF_MIN_SECS, BACKOFF_MAX_SECS)
}

/// Compute the MQTT device block for a given entity_id so that related entities
/// are grouped under the same Home Assistant device.
fn device_for_entity(id: &str) -> serde_json::Value {
    // Actuator entities: actuator_position_N, actuator_orientation_N, actuator_power_N,
    // actuator_N_travel_open, actuator_N_travel_close
    if let Some(rest) = id.strip_prefix("actuator_") {
        // Extract node id: strip known prefixes/suffixes
        let node_id = rest.strip_prefix("position_")
            .or_else(|| rest.strip_prefix("orientation_"))
            .or_else(|| rest.strip_prefix("power_"))
            .or_else(|| rest.strip_suffix("_travel_open"))
            .or_else(|| rest.strip_suffix("_travel_close"))
            .unwrap_or(rest);
        return json!({
            "name": format!("eSmart Actuator {node_id}"),
            "model": "eSmarter Client",
            "manufacturer": "eSmart",
            "identifiers": [format!("esmart_actuator_{node_id}")]
        });
    }
    // Room entities: room_temperature_N, room_setpoint_N, room_heating_on_N, room_heating_power_N
    if let Some(rest) = id.strip_prefix("room_") {
        let node_id = rest.strip_prefix("temperature_")
            .or_else(|| rest.strip_prefix("setpoint_"))
            .or_else(|| rest.strip_prefix("heating_on_"))
            .or_else(|| rest.strip_prefix("heating_power_"))
            .unwrap_or(rest);
        return json!({
            "name": format!("eSmart Room {node_id}"),
            "model": "eSmarter Client",
            "manufacturer": "eSmart",
            "identifiers": [format!("esmart_room_{node_id}")]
        });
    }
    // Fan entities: fan_speed_N, fan_mode_N, fan_power_N
    if let Some(rest) = id.strip_prefix("fan_") {
        let node_id = rest.strip_prefix("speed_")
            .or_else(|| rest.strip_prefix("mode_"))
            .or_else(|| rest.strip_prefix("power_"))
            .unwrap_or(rest);
        return json!({
            "name": format!("eSmart Fan {node_id}"),
            "model": "eSmarter Client",
            "manufacturer": "eSmart",
            "identifiers": [format!("esmart_fan_{node_id}")]
        });
    }
    // Valve entities: valve_power_N, valve_on_N
    if let Some(rest) = id.strip_prefix("valve_") {
        let node_id = rest.strip_prefix("power_")
            .or_else(|| rest.strip_prefix("on_"))
            .unwrap_or(rest);
        return json!({
            "name": format!("eSmart Valve {node_id}"),
            "model": "eSmarter Client",
            "manufacturer": "eSmart",
            "identifiers": [format!("esmart_valve_{node_id}")]
        });
    }
    // Unknown nodes (node_N_field) get one device per node
    if let Some(rest) = id.strip_prefix("node_") {
        let node_id = rest.split('_').next().unwrap_or(rest);
        return json!({
            "name": format!("eSmart Node {node_id}"),
            "model": "eSmarter Client",
            "manufacturer": "eSmart",
            "identifiers": [format!("esmart_node_{node_id}")]
        });
    }
    // Everything else (meters, totals, holiday_mode, freecooling, modbus errors,
    // valves_state, command errors) goes into a single "eSmart System" device.
    json!({
        "name": "eSmart System",
        "model": "eSmarter Client",
        "manufacturer": "eSmart",
        "identifiers": ["esmart_system"]
    })
}

/// Fields shared by every discovery payload: device grouping and availability.
fn base_config(entity_id: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("device".into(), device_for_entity(entity_id));
    map.insert("availability_topic".into(), json!(AVAILABILITY_TOPIC));
    map
}

fn send_stats<I: IntoIterator<Item = ChannelMessage>>(queue: &Sender<ChannelMessage>, stats: I) {
    for msg in stats {
        // Only fails when no receiver is subscribed, i.e. while MQTT is reconnecting
        if queue.send(msg).is_err() {
            log::debug!("No MQTT receiver for stat, dropping it");
        }
    }
}

fn process_meter(queue: &Sender<ChannelMessage>, meter: &Meter) {
    send_stats(queue, meter.into_stats_iter());
}

fn process_node(queue: &Sender<ChannelMessage>, node: &Node) {
    send_stats(queue, node.into_stats_iter());
}

async fn publish_retained(client: &AsyncClient, topic: String, payload: String) -> Result<(), ClientError> {
    client
        .publish(topic, rumqttc::QoS::AtLeastOnce, true, payload)
        .await
}

async fn send_mqtt_discovery(
    client: &mut AsyncClient,
    id: &str,
    stat: &Stat,
) -> Result<(), ClientError> {
    let topic = format!("homeassistant/sensor/esmart/{id}");

    let config_topic = format!("{topic}/config");
    let state_topic = format!("esmart/{id}/state");
    let name = format!("{} ({})", stat.name(), stat.property());

    let mut map = base_config(id);
    map.insert("name".into(), json!(name));
    map.insert("state_topic".into(), json!(state_topic));
    map.insert("icon".into(), json!(stat.icon()));
    // Bracket syntax so property names with odd characters still work
    map.insert("value_template".into(), json!(format!("{{{{ value_json['{}'] }}}}", stat.property())));
    map.insert("unique_id".into(), json!(format!("esmart_{id}")));
    if let Some(dc) = stat.device_class_str() {
        map.insert("device_class".into(), json!(dc));
    }
    if let Some(unit) = stat.unit_str() {
        map.insert("unit_of_measurement".into(), json!(unit));
    }
    if let Some(sc) = stat.state_class_str() {
        map.insert("state_class".into(), json!(sc));
    }
    let json = serde_json::Value::Object(map);

    log::debug!("Sending discovery message to '{}': {}", config_topic, json);
    publish_retained(client, config_topic, json.to_string()).await
}

async fn send_mqtt_update(
    client: &mut AsyncClient,
    id: &str,
    property: &str,
    value: &StatValue,
) -> Result<(), ClientError> {
    let topic = format!("esmart/{id}/state");
    let json = match value {
        StatValue::Float(v) => json!({ property: v }),
        StatValue::Text(v) => json!({ property: v }),
    };

    log::debug!("Sending data payload to '{}': {}", topic, json);
    // Totals are retained so HA gets the current value right after a restart
    let retain = id.starts_with("energy_") || id.starts_with("water_");
    client
        .publish(topic, rumqttc::QoS::AtLeastOnce, retain, json.to_string())
        .await?;

    Ok(())
}

struct XmppContext {
    sender: Sender<ChannelMessage>,
    args: Arc<cli::Cli>,
    messages_recv: Arc<AtomicU64>,
    totals: totals::SharedTotals,
    /// Unix timestamp of the last eSmart data message, read by the availability watchdog.
    last_data: Arc<AtomicI64>,
}

/// Publish the result of an eSmart error response as a sensor, so failed
/// commands are visible in Home Assistant and not only in the journal.
fn report_command_error(ctx: &XmppContext, error: &str, last_command: &Option<String>) {
    let text = match last_command {
        Some(cmd) => format!("{error} ({cmd})"),
        None => error.to_string(),
    };
    send_stats(
        &ctx.sender,
        [(
            "last_command_error".to_string(),
            Stat::new("Last Command Error", Unit::None, DeviceClass::None, "mdi:alert", "error"),
            StatValue::Text(text),
        )],
    );
}

/// Returns Ok(true) if data was received during this session, so the caller
/// can reset its reconnect backoff.
async fn xmpp_task(
    ctx: &XmppContext,
    cmd_receiver: &mut mpsc::Receiver<commands::ESmartCommand>,
) -> Result<bool, Error> {
    log::info!("Started XMPP task");
    let args = &ctx.args;

    let mut client = Client::new(
        tokio_xmpp::jid::BareJid::new(&args.xmpp_jid.to_string())?,
        args.xmpp_password.clone(),
    );

    let stale_after = Duration::from_secs(args.xmpp_stale_secs);
    let mut watchdog = tokio::time::interval(Duration::from_secs(10));
    let mut last_activity = tokio::time::Instant::now();
    let mut got_data = false;
    let mut last_command: Option<String> = None;

    loop {
        tokio::select! {
            elem = client.next() => {
                let Some(elem) = elem else { break };
                match elem {
                    Event::Stanza(stanza) => match Message::try_from(stanza) {
                        Ok(message) => {
                            if let Some((_, body)) = message.get_best_body(vec![""]) {
                                log::debug!("Raw XMPP message body: {}", body);
                                // Skip non-JSON messages (e.g. MUC room invitations)
                                let body_trimmed = body.trim();
                                if !body_trimmed.starts_with('{') {
                                    log::debug!("Ignoring non-JSON XMPP message");
                                    continue;
                                }
                                // Pre-parse to check headers before full deserialization
                                let envelope = match serde_json::from_str::<serde_json::Value>(body) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        log::warn!("Failed to parse XMPP message as JSON: {:?}", e);
                                        continue;
                                    }
                                };
                                let method = envelope.pointer("/headers/method").and_then(|v| v.as_str());
                                let msg_type = envelope.pointer("/headers/type").and_then(|v| v.as_str());
                                // Only INFO/data messages contain the full payload we need
                                if method != Some("INFO") {
                                    // CMD messages sent by the official app show up here too;
                                    // logged at info so new commands can be learned from them.
                                    // Echoes of our own commands only at debug.
                                    let from = envelope.pointer("/headers/from").and_then(|v| v.as_str());
                                    if from == Some(args.xmpp_jid.to_string().as_str()) {
                                        log::debug!("Echo of own command: {}", body_trimmed);
                                    } else {
                                        log::info!("Message from another client (method={:?}): {}", method, body_trimmed);
                                    }
                                    continue;
                                }
                                if msg_type != Some("data") {
                                    // Error responses, operation echoes, etc.
                                    if let Some(error) = envelope.pointer("/error/message") {
                                        let error = error.as_str().map(|s| s.to_string()).unwrap_or_else(|| error.to_string());
                                        log::warn!(
                                            "eSmart error response: {} (last command: {})",
                                            error,
                                            last_command.as_deref().unwrap_or("none")
                                        );
                                        report_command_error(ctx, &error, &last_command);
                                    } else {
                                        log::debug!("Ignoring INFO message with type={:?}", msg_type);
                                    }
                                    continue;
                                }
                                match serde_json::from_str::<data::ESmartMessage>(body) {
                                    Ok(msg) => {
                                        got_data = true;
                                        last_activity = tokio::time::Instant::now();
                                        ctx.last_data.store(Utc::now().timestamp(), Ordering::Release);
                                        ctx.messages_recv.fetch_add(1, Ordering::AcqRel);
                                        let now = Utc::now();
                                        msg.iter_meters()
                                            .for_each(|meter| process_meter(&ctx.sender, meter));
                                        // Cumulative energy/water, computed on every message
                                        // (not throttled) so the integration stays accurate
                                        let cumulative: Vec<ChannelMessage> = match ctx.totals.lock() {
                                            Ok(mut t) => msg.iter_meters().filter_map(|m| t.process_meter(now, m)).collect(),
                                            Err(_) => vec![],
                                        };
                                        send_stats(&ctx.sender, cumulative);
                                        msg.iter_nodes()
                                            .for_each(|node| process_node(&ctx.sender, node));
                                        send_stats(&ctx.sender, msg.body_stats());
                                    }
                                    Err(e) => log::warn!("Failed to parse ESmartMessage: {:?} body: {}", e, body_trimmed),
                                }
                            }
                        }
                        Err(_) => continue,
                    },
                    Event::Disconnected(e) => {
                        log::error!("XMPP client disconnected: {:?}", e);
                        return Err(Error::Disconnected);
                    }
                    Event::Online { .. } => {
                        log::info!("XMPP online, joining room {}", args.xmpp_room);
                        last_activity = tokio::time::Instant::now();
                        let mut presence = Presence::new(PresenceType::None);
                        presence.show = Some(PresenceShow::Chat);
                        client.send_stanza(presence.into()).await?;

                        let muc = Muc::new();
                        let room_jid = match args
                            .xmpp_room
                            .with_resource_str(&args.xmpp_nickname)
                        {
                            Ok(jid) => jid,
                            Err(e) => {
                                log::error!(
                                    "Failed to construct room JID with nickname '{}': {:?}",
                                    args.xmpp_nickname,
                                    e
                                );
                                continue;
                            }
                        };
                        let mut presence = Presence::new(PresenceType::None).with_to(room_jid);
                        presence.add_payload(muc);
                        presence.set_status("en", "here");
                        if let Err(e) = client.send_stanza(presence.into()).await {
                            log::error!("Failed to join XMPP room: {:?}", e);
                            return Err(e.into());
                        }
                    }
                }
            }
            cmd = cmd_receiver.recv() => {
                let Some(cmd) = cmd else {
                    log::warn!("Command channel closed");
                    break;
                };
                let age = cmd.created.elapsed();
                if age > Duration::from_secs(args.command_timeout_secs) {
                    log::warn!("Dropping command queued {}s ago (XMPP was unavailable): {}", age.as_secs(), cmd.description);
                    continue;
                }
                log::info!("Sending command to eSmart: {}", cmd.description);
                log::debug!("Command XMPP payload: {}", cmd.xmpp_payload);
                let mut message = Message::new(Some(args.xmpp_room.clone().into()));
                message.type_ = MessageType::Groupchat;
                message.bodies.insert(
                    xmpp_parsers::message::Lang(String::new()),
                    cmd.xmpp_payload.clone(),
                );
                if let Err(e) = client.send_stanza(message.into()).await {
                    log::error!("Failed to send XMPP command '{}': {:?}", cmd.description, e);
                    report_command_error(ctx, "not delivered, XMPP send failed", &Some(cmd.description));
                    return Err(e.into());
                }
                last_command = Some(cmd.description);
            }
            _ = watchdog.tick() => {
                // eSmart pushes data every second or two; silence means the
                // session is dead even if the TCP connection looks fine.
                if last_activity.elapsed() > stale_after {
                    log::error!(
                        "No eSmart data for {}s, forcing XMPP reconnect",
                        last_activity.elapsed().as_secs()
                    );
                    return Err(Error::Disconnected);
                }
            }
        }
    }

    Ok(got_data)
}

async fn send_cover_discovery(
    client: &mut AsyncClient,
    node_id: u16,
) -> Result<(), ClientError> {
    let id = format!("actuator_{node_id}");
    let config_topic = format!("homeassistant/cover/esmart/{id}/config");
    // Position is already scaled to 0-100 and direction-inverted in data.rs.
    // Tilt (orientation) is also scaled to 0-100 and inverted in data.rs.
    // device_class "shutter" gives the open/stop/close buttons UI in HA.
    let mut config = base_config(&format!("actuator_position_{node_id}"));
    config.extend(
        json!({
            "name": format!("Actuator {node_id}"),
            "unique_id": format!("esmart_actuator_{node_id}"),
            "device_class": "shutter",
            "command_topic": format!("esmart/actuator_{node_id}_position/set"),
            "position_topic": format!("esmart/actuator_position_{node_id}/state"),
            "position_template": "{{ value_json.position | int }}",
            "set_position_topic": format!("esmart/actuator_{node_id}_position/set"),
            "position_open": 100,
            "position_closed": 0,
            "payload_open": "OPEN",
            "payload_close": "CLOSE",
            "payload_stop": "STOP",
            "tilt_status_topic": format!("esmart/actuator_orientation_{node_id}/state"),
            "tilt_status_template": "{{ value_json.orientation | int }}",
            "tilt_command_topic": format!("esmart/actuator_{node_id}_tilt/set"),
            "tilt_opened_value": 100,
            "tilt_closed_value": 0
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    log::debug!("Sending cover discovery to '{}': {:?}", config_topic, config);
    publish_retained(client, config_topic, serde_json::Value::Object(config).to_string()).await
}

async fn send_climate_discovery(
    client: &mut AsyncClient,
    node_id: u16,
) -> Result<(), ClientError> {
    let id = format!("room_{node_id}");
    let config_topic = format!("homeassistant/climate/esmart/{id}/config");
    let mut config = base_config(&format!("room_temperature_{node_id}"));
    config.extend(
        json!({
            "name": format!("Room {node_id}"),
            "unique_id": format!("esmart_climate_{node_id}"),
            "icon": "mdi:radiator",
            "modes": ["off", "heat"],
            "mode_state_template": "{% if value_json.deviceOnOff == 1.0 %}heat{% else %}off{% endif %}",
            "mode_state_topic": format!("esmart/room_heating_on_{node_id}/state"),
            "temperature_command_topic": format!("esmart/room_{node_id}_setpoint/set"),
            "temperature_state_topic": format!("esmart/room_setpoint_{node_id}/state"),
            "temperature_state_template": "{{ value_json.setpoint }}",
            "current_temperature_topic": format!("esmart/room_temperature_{node_id}/state"),
            "current_temperature_template": "{{ value_json.temperature }}",
            "min_temp": 18,
            "max_temp": 24,
            "temp_step": 1,
            "temperature_unit": "C"
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    log::debug!("Sending climate discovery to '{}': {:?}", config_topic, config);
    publish_retained(client, config_topic, serde_json::Value::Object(config).to_string()).await
}

async fn send_fan_discovery(
    client: &mut AsyncClient,
    node_id: u16,
) -> Result<(), ClientError> {
    let id = format!("fan_{node_id}");
    let config_topic = format!("homeassistant/fan/esmart/{id}/config");
    // The device only accepts speeds 0%/33%/66%/99% and reports power 0 even
    // while running, so on/off and speed are both derived from "speed".
    // HA shows three steps (speed_range 1-3) and sends the step on fan_N_level.
    let mut config = base_config(&format!("fan_speed_{node_id}"));
    config.extend(
        json!({
            "name": format!("Fan {node_id}"),
            "unique_id": format!("esmart_fan_{node_id}"),
            "icon": "mdi:fan",
            "command_topic": format!("esmart/fan_{node_id}_onoff/set"),
            "state_topic": format!("esmart/fan_speed_{node_id}/state"),
            "state_value_template": "{% if value_json.speed | replace('%','') | int(0) > 0 %}ON{% else %}OFF{% endif %}",
            "percentage_command_topic": format!("esmart/fan_{node_id}_level/set"),
            "percentage_state_topic": format!("esmart/fan_speed_{node_id}/state"),
            "percentage_value_template": "{{ ((value_json.speed | replace('%','') | int(0)) / 33) | round(0) | int }}",
            "speed_range_min": 1,
            "speed_range_max": 3
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    log::debug!("Sending fan discovery to '{}': {:?}", config_topic, config);
    publish_retained(client, config_topic, serde_json::Value::Object(config).to_string()).await
}

async fn send_switch_discovery(
    client: &mut AsyncClient,
    entity_id: &str,
    name: &str,
    icon: &str,
    state_property: &str,
) -> Result<(), ClientError> {
    let config_topic = format!("homeassistant/switch/esmart/{entity_id}/config");
    let value_tpl = format!(
        "{{% if value_json.{} == 1.0 %}}ON{{% else %}}OFF{{% endif %}}",
        state_property
    );
    let mut config = base_config(entity_id);
    config.extend(
        json!({
            "name": name,
            "unique_id": format!("esmart_{entity_id}"),
            "icon": icon,
            "command_topic": format!("esmart/{entity_id}/set"),
            "state_topic": format!("esmart/{entity_id}/state"),
            "value_template": value_tpl,
            "payload_on": "ON",
            "payload_off": "OFF"
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    log::debug!("Sending switch discovery to '{}': {:?}", config_topic, config);
    publish_retained(client, config_topic, serde_json::Value::Object(config).to_string()).await
}

async fn send_binary_sensor_discovery(
    client: &mut AsyncClient,
    entity_id: &str,
    name: &str,
    icon: &str,
    state_property: &str,
) -> Result<(), ClientError> {
    let config_topic = format!("homeassistant/binary_sensor/esmart/{entity_id}/config");
    let value_tpl = format!(
        "{{% if value_json.{} == 1.0 %}}ON{{% else %}}OFF{{% endif %}}",
        state_property
    );
    let mut config = base_config(entity_id);
    config.extend(
        json!({
            "name": name,
            "unique_id": format!("esmart_{entity_id}_binary"),
            "icon": icon,
            "state_topic": format!("esmart/{entity_id}/state"),
            "value_template": value_tpl,
            "payload_on": "ON",
            "payload_off": "OFF"
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    log::debug!("Sending binary_sensor discovery to '{}': {:?}", config_topic, config);
    publish_retained(client, config_topic, serde_json::Value::Object(config).to_string()).await
}

#[allow(clippy::too_many_arguments)]
async fn send_number_discovery(
    client: &mut AsyncClient,
    entity_id: &str,
    name: &str,
    icon: &str,
    min: f32,
    max: f32,
    step: f32,
    unit: &str,
    initial: f32,
) -> Result<(), ClientError> {
    let config_topic = format!("homeassistant/number/esmart/{entity_id}/config");
    let mut config = base_config(entity_id);
    config.extend(
        json!({
            "name": name,
            "unique_id": format!("esmart_{entity_id}"),
            "icon": icon,
            "command_topic": format!("esmart/{entity_id}/set"),
            "state_topic": format!("esmart/{entity_id}/state"),
            "min": min,
            "max": max,
            "step": step,
            "unit_of_measurement": unit,
            "mode": "box"
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    log::debug!("Sending number discovery to '{}': {:?}", config_topic, config);
    publish_retained(client, config_topic, serde_json::Value::Object(config).to_string()).await?;
    // Publish the current (persisted or default) value so HA shows it
    publish_retained(client, format!("esmart/{entity_id}/state"), initial.to_string()).await
}

/// Discovery for the always-present control entities that are not tied to a sensor id.
async fn send_static_discovery(client: &mut AsyncClient) {
    let config_topic = "homeassistant/sensor/esmart/last_command_error/config".to_string();
    let mut config = base_config("last_command_error");
    config.extend(
        json!({
            "name": "Last Command Error",
            "unique_id": "esmart_last_command_error",
            "icon": "mdi:alert",
            "state_topic": "esmart/last_command_error/state",
            "value_template": "{{ value_json.error }}",
            "entity_category": "diagnostic"
        })
        .as_object()
        .cloned()
        .unwrap_or_default(),
    );
    if let Err(e) = publish_retained(client, config_topic, serde_json::Value::Object(config).to_string()).await {
        log::error!("Error sending last_command_error discovery: {:?}", e);
    }
}

struct MqttContext {
    cmd_sender: mpsc::Sender<commands::ESmartCommand>,
    args: Arc<cli::Cli>,
    messages_sent: Arc<AtomicU64>,
    totals: totals::SharedTotals,
    last_data: Arc<AtomicI64>,
    actuator_states: commands::ActuatorStates,
    fan_states: commands::FanStates,
}

// task which fetches the stats from the queue and sends them to MQTT
async fn mqtt_task(mut receiver: Receiver<ChannelMessage>, ctx: &MqttContext) -> bool {
    let args = &ctx.args;
    let mut last_contact: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut cache_queues: HashMap<String, Vec<StatValue>> = HashMap::new();

    let mut options = MqttOptions::new(&args.mqtt_id, &args.mqtt_hostname, args.mqtt_port);

    log::info!("Started MQTT task");

    if let Some(username) = &args.mqtt_username {
        match &args.mqtt_password {
            Some(password) => {
                options.set_credentials(username, password);
                log::info!("MQTT: connecting with authentication (user: {})", username);
            }
            None => {
                log::error!("MQTT username is set but password is missing!");
                return false;
            }
        }
    } else {
        log::info!("MQTT: connecting without authentication");
    }

    options.set_keep_alive(Duration::from_secs(5));
    options.set_last_will(LastWill::new(
        AVAILABILITY_TOPIC,
        "offline",
        rumqttc::QoS::AtLeastOnce,
        true,
    ));

    let (client, mut event_loop) = AsyncClient::new(options, 100);

    let mqtt_throttling_secs = args.mqtt_throttling_secs as i64;
    let jid = args.xmpp_jid.to_string();

    // Set by the event loop when HA restarts, so discovery is sent again
    let resend_discovery = Arc::new(AtomicBool::new(false));
    let resend_flag = resend_discovery.clone();

    let actuator_states_writer = ctx.actuator_states.clone();
    let fan_states_writer = ctx.fan_states.clone();
    let totals_reader = ctx.totals.clone();
    let messages_sent = ctx.messages_sent.clone();

    // spawn a task to handle incoming messages from the XMPP client
    let mut client_clone = client.clone();
    let receiver_handle = tokio::spawn(async move {
        // Track which entities we've already sent discovery for
        let mut control_discovery_sent: HashSet<String> = HashSet::new();
        send_static_discovery(&mut client_clone).await;
        loop {
            if resend_flag.swap(false, Ordering::AcqRel) {
                log::info!("Home Assistant restarted, re-sending discovery");
                control_discovery_sent.clear();
                cache_queues.clear();
                last_contact.clear();
                send_static_discovery(&mut client_clone).await;
            }
            match receiver.recv().await {
                Ok((id, stat, value)) => {
                    log::trace!("Processing update for {id}");

                    // Track actuator positions and orientations (HA 0-100 scale) for STOP estimation.
                    // NOTE: Do NOT clear in_flight here. The device immediately reports
                    // the *target* position while the shutter is still physically moving.
                    // in_flight is only cleared by STOP or by a new move command.
                    if let Some(node_id) = id.strip_prefix("actuator_position_").and_then(|s| s.parse::<u16>().ok()) {
                        if let StatValue::Float(pos) = &value {
                            if let Ok(mut states) = actuator_states_writer.lock() {
                                let state = states.entry(node_id).or_insert_with(commands::ActuatorState::new);
                                state.ha_position = *pos as f32;
                            }
                        }
                    }
                    if let Some(node_id) = id.strip_prefix("actuator_orientation_").and_then(|s| s.parse::<u16>().ok()) {
                        if let StatValue::Float(orient) = &value {
                            if let Ok(mut states) = actuator_states_writer.lock() {
                                let state = states.entry(node_id).or_insert_with(commands::ActuatorState::new);
                                state.ha_orientation = *orient as f32;
                            }
                        }
                    }
                    // Remember the last non-zero fan speed so ON can restore it
                    if let Some(node_id) = id.strip_prefix("fan_speed_").and_then(|s| s.parse::<u16>().ok()) {
                        if let StatValue::Text(speed) = &value {
                            if speed != commands::FAN_SPEEDS[0] && commands::FAN_SPEEDS.contains(&speed.as_str()) {
                                if let Ok(mut fans) = fan_states_writer.lock() {
                                    fans.insert(node_id, speed.clone());
                                }
                            }
                        }
                    }

                    // Send control entity discovery on first sight of controllable nodes
                    if !control_discovery_sent.contains(&id) {
                        if let Some(node_id) = id.strip_prefix("room_temperature_").and_then(|s| s.parse::<u16>().ok()) {
                            if let Err(e) = send_climate_discovery(&mut client_clone, node_id).await {
                                log::error!("Error sending climate discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        } else if let Some(node_id) = id.strip_prefix("actuator_position_").and_then(|s| s.parse::<u16>().ok()) {
                            if let Err(e) = send_cover_discovery(&mut client_clone, node_id).await {
                                log::error!("Error sending cover discovery: {:?}", e);
                            }
                            // Travel times: persisted value if any, else the 30 s default
                            let (open_s, close_s) = match totals_reader.lock() {
                                Ok(t) => (
                                    t.travel_time(node_id, "open").unwrap_or(30.0) as f32,
                                    t.travel_time(node_id, "close").unwrap_or(30.0) as f32,
                                ),
                                Err(_) => (30.0, 30.0),
                            };
                            if let Ok(mut states) = actuator_states_writer.lock() {
                                let state = states.entry(node_id).or_insert_with(commands::ActuatorState::new);
                                state.travel_time_open = open_s;
                                state.travel_time_close = close_s;
                            }
                            // Send Number entity discovery for travel time configuration
                            if let Err(e) = send_number_discovery(
                                &mut client_clone,
                                &format!("actuator_{node_id}_travel_open"),
                                &format!("Actuator {node_id} Open Travel Time"),
                                "mdi:timer",
                                1.0, 120.0, 1.0, "s", open_s,
                            ).await {
                                log::error!("Error sending travel_open number discovery: {:?}", e);
                            }
                            if let Err(e) = send_number_discovery(
                                &mut client_clone,
                                &format!("actuator_{node_id}_travel_close"),
                                &format!("Actuator {node_id} Close Travel Time"),
                                "mdi:timer",
                                1.0, 120.0, 1.0, "s", close_s,
                            ).await {
                                log::error!("Error sending travel_close number discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        } else if let Some(node_id) = id.strip_prefix("fan_speed_").and_then(|s| s.parse::<u16>().ok()) {
                            if let Err(e) = send_fan_discovery(&mut client_clone, node_id).await {
                                log::error!("Error sending fan discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        } else if id == "holiday_mode" {
                            if let Err(e) = send_switch_discovery(&mut client_clone, "holiday_mode", "Holiday Mode", "mdi:beach", "holiday_mode").await {
                                log::error!("Error sending holiday mode switch discovery: {:?}", e);
                            }
                            if let Err(e) = send_binary_sensor_discovery(&mut client_clone, "holiday_mode", "Holiday Mode", "mdi:beach", "holiday_mode").await {
                                log::error!("Error sending holiday mode binary_sensor discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        } else if id == "freecooling" {
                            if let Err(e) = send_switch_discovery(&mut client_clone, "freecooling", "Freecooling", "mdi:snowflake", "freecooling").await {
                                log::error!("Error sending freecooling switch discovery: {:?}", e);
                            }
                            if let Err(e) = send_binary_sensor_discovery(&mut client_clone, "freecooling", "Freecooling", "mdi:snowflake", "freecooling").await {
                                log::error!("Error sending freecooling binary_sensor discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        }
                    }

                    let value_clone = value.clone();
                    match cache_queues.get_mut(&id) {
                        Some(queue) => queue.push(value),
                        None => {
                            // Skip redundant numeric sensor for entities that have
                            // switch + binary_sensor discovery, and for the error
                            // sensor, whose discovery is static
                            if id != "freecooling" && id != "holiday_mode" && id != "last_command_error" {
                                if let Err(e) = send_mqtt_discovery(&mut client_clone, &id, &stat).await
                                {
                                    log::error!("Error sending discovery message: {:?}", e);
                                }
                            }
                            cache_queues.insert(id.clone(), vec![value]);
                        }
                    }

                    // Errors are events: always publish them immediately
                    let send_message = id == "last_command_error" || match last_contact.get(&id) {
                        Some(ts) => {
                            Utc::now() > (*ts + ChronoDuration::seconds(mqtt_throttling_secs))
                        }
                        None => true,
                    };

                    if send_message {
                        let Some(queue) = cache_queues.get_mut(&id) else { continue };
                        // Always use the latest value: averaging causes ghost values
                        // for control states (setpoint, deviceOnOff, position).
                        let resolved = queue.last().cloned().unwrap_or(value_clone);

                        queue.clear();
                        let _ = last_contact.insert(id.clone(), Utc::now());
                        if let Err(e) =
                            send_mqtt_update(&mut client_clone, &id, stat.property(), &resolved).await
                        {
                            log::error!("Error sending update message: {:?}", e);
                        } else {
                            messages_sent.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    log::error!("XMPP half seems to have died!");
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("MQTT receiver lagging behind, skipped {n} updates");
                }
            }
        }
    });

    // Availability watchdog: online only while eSmart data is fresh
    let avail_client = client.clone();
    let last_data = ctx.last_data.clone();
    let stale_secs = args.xmpp_stale_secs as i64;
    let availability_handle = tokio::spawn(async move {
        let mut published: Option<bool> = None;
        loop {
            let age = Utc::now().timestamp() - last_data.load(Ordering::Acquire);
            let online = age <= stale_secs;
            if published != Some(online) {
                let payload = if online { "online" } else { "offline" };
                log::info!("Publishing availability: {payload}");
                if publish_retained(&avail_client, AVAILABILITY_TOPIC.into(), payload.into()).await.is_ok() {
                    published = Some(online);
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });

    // Main MQTT event loop: handle notifications including incoming Publish (commands from HA)
    let cmd_client = client.clone();
    let mut connected = false;
    loop {
        match event_loop.poll().await {
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                connected = true;
                log::info!("MQTT connected, subscribing to command topics");
                for topic in ["esmart/+/set", HA_STATUS_TOPIC] {
                    if let Err(e) = cmd_client.subscribe(topic, rumqttc::QoS::AtLeastOnce).await {
                        log::error!("Failed to subscribe to {}: {:?}", topic, e);
                    } else {
                        log::info!("Subscribed to {}", topic);
                    }
                }
            }
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(publish))) => {
                let topic = publish.topic.clone();
                let payload = match std::str::from_utf8(&publish.payload) {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        log::warn!("Non-UTF8 payload on topic {}", topic);
                        continue;
                    }
                };
                if topic == HA_STATUS_TOPIC {
                    if payload.trim() == "online" {
                        resend_discovery.store(true, Ordering::Release);
                    }
                    continue;
                }
                log::info!("Received MQTT command: topic={} payload={}", topic, payload);
                match commands::parse_mqtt_command(&jid, &topic, &payload, &ctx.actuator_states, &ctx.fan_states) {
                    commands::ParseResult::Command(cmd) => {
                        if let Err(e) = ctx.cmd_sender.try_send(cmd) {
                            log::error!("Command queue full or closed, dropping command: {:?}", e);
                        }
                    }
                    commands::ParseResult::TravelTime { node_id, dir, secs } => {
                        if let Ok(mut t) = ctx.totals.lock() {
                            t.set_travel_time(node_id, dir, secs as f64);
                            t.save();
                        }
                        // Publish updated travel time state back to MQTT so HA reflects the value
                        if let Err(e) = publish_retained(
                            &cmd_client,
                            topic.replace("/set", "/state"),
                            secs.to_string(),
                        ).await {
                            log::error!("Failed to publish travel time state: {:?}", e);
                        }
                    }
                    commands::ParseResult::Unrecognized => {
                        log::warn!("Could not parse command from topic={} payload={}", topic, payload);
                    }
                }
            }
            Ok(notification) => {
                log::trace!("Received = {:?}", notification);
            }
            Err(e) => {
                log::error!("MQTT event loop error: {:?}", e);
                break;
            }
        }
    }

    // If we exit the event loop, abort the helper tasks and return
    receiver_handle.abort();
    availability_handle.abort();
    log::warn!("MQTT event loop exited, task will return for reconnect.");
    connected
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("Cannot listen for SIGTERM: {:?}", e);
                let _ = ctrl_c.await;
                return;
            }
        };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}

#[tokio::main]
async fn main() {
    let args = Arc::new(cli::Cli::parse());

    env_logger::init();

    let totals = totals::Totals::load(args.state_file.as_deref()).shared();

    let (sender, receiver) = broadcast::channel(1024);

    let messages_sent = Arc::new(AtomicU64::new(0));
    let messages_recv = Arc::new(AtomicU64::new(0));
    let last_data = Arc::new(AtomicI64::new(0));

    let ms = messages_sent.clone();
    let mr = messages_recv.clone();
    // task to report statistics every now and then
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            log::info!(
                "Messages: {} received / {} sent",
                mr.load(Ordering::Acquire),
                ms.load(Ordering::Acquire)
            );
        }
    });

    // task to persist totals every minute
    let totals_saver = totals.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Ok(mut t) = totals_saver.lock() {
                t.save();
            }
        }
    });

    // Command channel: MQTT task sends commands, XMPP task receives and forwards to eSmart
    let (cmd_sender, cmd_receiver) = mpsc::channel::<commands::ESmartCommand>(64);

    // Spawn XMPP task with exponential backoff
    let xmpp_ctx = XmppContext {
        sender,
        args: args.clone(),
        messages_recv,
        totals: totals.clone(),
        last_data: last_data.clone(),
    };
    let xmpp_handle = tokio::spawn(async move {
        let mut cmd_rx = cmd_receiver;
        let mut backoff = BACKOFF_MIN_SECS;
        loop {
            log::info!("Starting XMPP task");
            match xmpp_task(&xmpp_ctx, &mut cmd_rx).await {
                Ok(true) => backoff = BACKOFF_MIN_SECS,
                Ok(false) => backoff = next_backoff(backoff),
                Err(e) => {
                    log::error!("XMPP task failed: {:?}", e);
                    // A session that delivered data was healthy; start over
                    if Utc::now().timestamp() - xmpp_ctx.last_data.load(Ordering::Acquire)
                        <= xmpp_ctx.args.xmpp_stale_secs as i64 + 10
                    {
                        backoff = BACKOFF_MIN_SECS;
                    } else {
                        backoff = next_backoff(backoff);
                    }
                }
            }
            log::info!("Reconnecting to XMPP in {backoff}s");
            tokio::time::sleep(Duration::from_secs(backoff)).await;
        }
    });

    // Spawn MQTT task with exponential backoff
    let mqtt_ctx = MqttContext {
        cmd_sender,
        args: args.clone(),
        messages_sent,
        totals: totals.clone(),
        last_data,
        actuator_states: commands::new_actuator_states(),
        fan_states: commands::new_fan_states(),
    };
    let mqtt_handle = tokio::spawn(async move {
        let mut backoff = BACKOFF_MIN_SECS;
        loop {
            log::info!("Starting MQTT task");
            let was_connected = mqtt_task(receiver.resubscribe(), &mqtt_ctx).await;
            backoff = if was_connected { BACKOFF_MIN_SECS } else { next_backoff(backoff) };
            log::warn!("MQTT task failed or disconnected, reconnecting in {backoff}s...");
            tokio::time::sleep(Duration::from_secs(backoff)).await;
        }
    });

    // Wait for a shutdown signal or for either task to finish
    tokio::select! {
        _ = xmpp_handle => {
            log::warn!("XMPP task exited, shutting down.");
        }
        _ = mqtt_handle => {
            log::warn!("MQTT task exited, shutting down.");
        }
        _ = shutdown_signal() => {
            log::info!("Shutdown signal received");
        }
    }

    let saved = totals.lock().map(|mut t| t.save());
    if saved.is_err() {
        log::error!("Could not save state on shutdown: lock poisoned");
    }
}
