use std::{
    collections::HashMap,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use clap::Parser;
use data::{Meter, Node};
use rumqttc::{AsyncClient, ClientError, MqttOptions};
use serde_json::json;
use stats::{IterStats, Stat, StatValue};
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

type ChannelMessage = (String, Stat, StatValue);

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
    // Everything else (meters, holiday_mode, freecooling, modbus errors, valves_state)
    // goes into a single "eSmart System" device.
    json!({
        "name": "eSmart System",
        "model": "eSmarter Client",
        "manufacturer": "eSmart",
        "identifiers": ["esmart_system"]
    })
}

fn process_meter(queue: &mut Sender<ChannelMessage>, meter: &Meter) {
    for (id, stat, value) in meter.into_stats_iter() {
        match queue.send((id, stat, value)) {
            Ok(_) => {}
            Err(_) => log::error!("This shouldn't happen!"),
        }
    }
}

fn process_node(queue: &mut Sender<ChannelMessage>, node: &Node) {
    for (id, stat, value) in node.into_stats_iter() {
        match queue.send((id, stat, value)) {
            Ok(_) => {}
            Err(_) => log::error!("This shouldn't happen!"),
        }
    }
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

    let mut map = serde_json::Map::new();
    map.insert("name".into(), json!(name));
    map.insert("state_topic".into(), json!(state_topic));
    map.insert("icon".into(), json!(stat.icon()));
    map.insert("value_template".into(), json!(format!("{{{{ value_json.{} }}}}", stat.property())));
    map.insert("unique_id".into(), json!(format!("esmart_{id}")));
    map.insert("device".into(), device_for_entity(id));
    if let Some(dc) = stat.device_class_str() {
        map.insert("device_class".into(), json!(dc));
    }
    if let Some(unit) = stat.unit_str() {
        map.insert("unit_of_measurement".into(), json!(unit));
    }
    let json = serde_json::Value::Object(map);

    log::debug!("Sending discovery message to '{}': {}", config_topic, json);
    client
        .publish(
            config_topic,
            rumqttc::QoS::AtLeastOnce,
            true,
            json.to_string(),
        )
        .await?;

    Ok(())
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
    client
        .publish(topic, rumqttc::QoS::AtLeastOnce, false, json.to_string())
        .await?;

    Ok(())
}

async fn xmpp_task(
    mut sender: Sender<ChannelMessage>,
    cmd_receiver: &mut mpsc::Receiver<commands::ESmartCommand>,
    args: Arc<cli::Cli>,
    messages_recv: Arc<AtomicU64>,
) -> Result<(), Error> {
    log::info!("Started XMPP task");

    let mut client = Client::new(
        tokio_xmpp::jid::BareJid::new(&args.xmpp_jid.to_string())?,
        args.xmpp_password.clone(),
    );

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
                                    log::debug!("Ignoring non-INFO message (method={:?})", method);
                                    continue;
                                }
                                if msg_type != Some("data") {
                                    // Error responses, operation echoes, etc.
                                    if let Some(error) = envelope.pointer("/error/message") {
                                        log::warn!("eSmart error response: {}", error);
                                    } else {
                                        log::debug!("Ignoring INFO message with type={:?}", msg_type);
                                    }
                                    continue;
                                }
                                match serde_json::from_str::<data::ESmartMessage>(body) {
                                    Ok(msg) => {
                                        messages_recv.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                                        msg.iter_meters()
                                            .for_each(|meter| process_meter(&mut sender, meter));
                                        msg.iter_nodes()
                                            .for_each(|node| process_node(&mut sender, node));
                                        for (id, stat, value) in msg.body_stats() {
                                            match sender.send((id, stat, value)) {
                                                Ok(_) => {}
                                                Err(_) => log::error!("Failed to send body stat"),
                                            }
                                        }
                                    }
                                    Err(e) => log::warn!("Failed to parse ESmartMessage: {:?}", e),
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
                        let _ = client.send_stanza(presence.into()).await;
                    }
                }
            }
            cmd = cmd_receiver.recv() => {
                let Some(cmd) = cmd else {
                    log::warn!("Command channel closed");
                    break;
                };
                log::info!("Sending command to eSmart: {}", cmd.description);
                log::debug!("Command XMPP payload: {}", cmd.xmpp_payload);
                let mut message = Message::new(Some(args.xmpp_room.clone().into()));
                message.type_ = MessageType::Groupchat;
                message.bodies.insert(
                    xmpp_parsers::message::Lang(String::new()),
                    cmd.xmpp_payload.clone(),
                );
                if let Err(e) = client.send_stanza(message.into()).await {
                    log::error!("Failed to send XMPP command: {:?}", e);
                }
            }
        }
    }

    Ok(())
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
    let config = json!({
        "name": format!("Actuator {node_id}"),
        "unique_id": format!("esmart_actuator_{node_id}"),
        "device": device_for_entity(&format!("actuator_position_{node_id}")),
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
    });
    log::debug!("Sending cover discovery to '{}': {}", config_topic, config);
    client
        .publish(config_topic, rumqttc::QoS::AtLeastOnce, true, config.to_string())
        .await
}

async fn send_climate_discovery(
    client: &mut AsyncClient,
    node_id: u16,
) -> Result<(), ClientError> {
    let id = format!("room_{node_id}");
    let config_topic = format!("homeassistant/climate/esmart/{id}/config");
    let config = json!({
        "name": format!("Room {node_id}"),
        "unique_id": format!("esmart_climate_{node_id}"),
        "device": device_for_entity(&format!("room_temperature_{node_id}")),
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
    });
    log::debug!("Sending climate discovery to '{}': {}", config_topic, config);
    client
        .publish(config_topic, rumqttc::QoS::AtLeastOnce, true, config.to_string())
        .await
}

async fn send_fan_discovery(
    client: &mut AsyncClient,
    node_id: u16,
) -> Result<(), ClientError> {
    let id = format!("fan_{node_id}");
    let config_topic = format!("homeassistant/fan/esmart/{id}/config");
    let config = json!({
        "name": format!("Fan {node_id}"),
        "unique_id": format!("esmart_fan_{node_id}"),
        "device": device_for_entity(&format!("fan_speed_{node_id}")),
        "icon": "mdi:fan",
        "command_topic": format!("esmart/fan_{node_id}_onoff/set"),
        "state_topic": format!("esmart/fan_power_{node_id}/state"),
        "state_value_template": "{% if value_json.power > 0 %}ON{% else %}OFF{% endif %}",
        "percentage_command_topic": format!("esmart/fan_{node_id}_speed/set"),
        "preset_mode_command_topic": format!("esmart/fan_{node_id}_mode/set"),
        "preset_modes": ["auto", "manual"]
    });
    log::debug!("Sending fan discovery to '{}': {}", config_topic, config);
    client
        .publish(config_topic, rumqttc::QoS::AtLeastOnce, true, config.to_string())
        .await
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
    let config = json!({
        "name": name,
        "unique_id": format!("esmart_{entity_id}"),
        "device": device_for_entity(entity_id),
        "icon": icon,
        "command_topic": format!("esmart/{entity_id}/set"),
        "state_topic": format!("esmart/{entity_id}/state"),
        "value_template": value_tpl,
        "payload_on": "ON",
        "payload_off": "OFF"
    });
    log::debug!("Sending switch discovery to '{}': {}", config_topic, config);
    client
        .publish(config_topic, rumqttc::QoS::AtLeastOnce, true, config.to_string())
        .await
}

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
    let config = json!({
        "name": name,
        "unique_id": format!("esmart_{entity_id}"),
        "device": device_for_entity(entity_id),
        "icon": icon,
        "command_topic": format!("esmart/{entity_id}/set"),
        "state_topic": format!("esmart/{entity_id}/state"),
        "min": min,
        "max": max,
        "step": step,
        "unit_of_measurement": unit,
        "mode": "box"
    });
    log::debug!("Sending number discovery to '{}': {}", config_topic, config);
    client
        .publish(
            config_topic,
            rumqttc::QoS::AtLeastOnce,
            true,
            config.to_string(),
        )
        .await?;
    // Publish initial state so HA shows the default value
    let state_topic = format!("esmart/{entity_id}/state");
    client
        .publish(
            state_topic,
            rumqttc::QoS::AtLeastOnce,
            true,
            initial.to_string(),
        )
        .await?;
    Ok(())
}

// task which fetches the stats from the queue and sends them to MQTT
async fn mqtt_task(
    mut receiver: Receiver<ChannelMessage>,
    cmd_sender: mpsc::Sender<commands::ESmartCommand>,
    args: Arc<cli::Cli>,
    messages_sent: Arc<AtomicU64>,
) {
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
                return;
            }
        }
    } else {
        log::info!("MQTT: connecting without authentication");
    }

    options.set_keep_alive(Duration::from_secs(5));

    let (client, mut event_loop) = AsyncClient::new(options, 10);

    let mqtt_throttling_secs = args.mqtt_throttling_secs as i64;
    let jid = args.xmpp_jid.to_string();

    // Track which control entities we've already sent discovery for
    let mut control_discovery_sent: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Shared actuator state for STOP position estimation and travel time config
    let actuator_states = commands::new_actuator_states();
    let actuator_states_writer = actuator_states.clone();
    let actuator_states_cmd = actuator_states.clone();

    // spawn a task to handle incoming messages from the XMPP client
    let mut client_clone = client.clone();
    let receiver_handle = tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok((id, stat, value)) => {
                    log::debug!("Processing update for {id}");

                    // Track actuator positions (HA 0-100 scale) for STOP estimation.
                    // NOTE: Do NOT clear in_flight here. The device immediately reports
                    // the *target* position while the shutter is still physically moving.
                    // in_flight is only cleared by STOP or by a new move command.
                    if let Some(node_id) = id.strip_prefix("actuator_position_").and_then(|s| s.parse::<u16>().ok()) {
                        if let StatValue::Float(pos) = &value {
                            if let Ok(mut states) = actuator_states_writer.lock() {
                                let state = states.entry(node_id).or_insert_with(commands::ActuatorState::new);
                                state.ha_position = *pos;
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
                            // Send Number entity discovery for travel time configuration
                            if let Err(e) = send_number_discovery(
                                &mut client_clone,
                                &format!("actuator_{node_id}_travel_open"),
                                &format!("Actuator {node_id} Open Travel Time"),
                                "mdi:timer",
                                1.0, 120.0, 1.0, "s", 30.0,
                            ).await {
                                log::error!("Error sending travel_open number discovery: {:?}", e);
                            }
                            if let Err(e) = send_number_discovery(
                                &mut client_clone,
                                &format!("actuator_{node_id}_travel_close"),
                                &format!("Actuator {node_id} Close Travel Time"),
                                "mdi:timer",
                                1.0, 120.0, 1.0, "s", 30.0,
                            ).await {
                                log::error!("Error sending travel_close number discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        } else if let Some(node_id) = id.strip_prefix("fan_power_").and_then(|s| s.parse::<u16>().ok()) {
                            if let Err(e) = send_fan_discovery(&mut client_clone, node_id).await {
                                log::error!("Error sending fan discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        } else if id == "holiday_mode" {
                            if let Err(e) = send_switch_discovery(&mut client_clone, "holiday_mode", "Holiday Mode", "mdi:beach", "holiday_mode").await {
                                log::error!("Error sending holiday mode switch discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        } else if id == "freecooling" {
                            if let Err(e) = send_switch_discovery(&mut client_clone, "freecooling", "Freecooling", "mdi:snowflake", "freecooling").await {
                                log::error!("Error sending freecooling switch discovery: {:?}", e);
                            }
                            control_discovery_sent.insert(id.clone());
                        }
                    }

                    let value_clone = value.clone();
                    match cache_queues.get_mut(&id) {
                        Some(queue) => queue.push(value),
                        None => {
                            if let Err(e) = send_mqtt_discovery(&mut client_clone, &id, &stat).await
                            {
                                log::error!("Error sending discovery message: {:?}", e);
                            }
                            cache_queues.insert(id.clone(), vec![value]);
                        }
                    }

                    let send_message = match last_contact.get(&id) {
                        Some(ts) => {
                            Utc::now() > (*ts + ChronoDuration::seconds(mqtt_throttling_secs))
                        }
                        None => true,
                    };

                    if send_message {
                        let queue = cache_queues.get_mut(&id).expect("cache queue missing after insert");
                        // Always use the latest value — averaging causes ghost values
                        // for control states (setpoint, deviceOnOff, position).
                        let resolved = queue.last().cloned().unwrap_or(value_clone);

                        queue.clear();
                        let _ = last_contact.insert(id.clone(), Utc::now());
                        if let Err(e) =
                            send_mqtt_update(&mut client_clone, &id, stat.property(), &resolved).await
                        {
                            log::error!("Error sending update message: {:?}", e);
                        } else {
                            messages_sent.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    log::error!("XMPP half seems to have died!");
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    log::error!("MQTT receiver lagging behind!");
                }
            }
        }
    });

    // Main MQTT event loop: handle notifications including incoming Publish (commands from HA)
    let cmd_client = client.clone();
    loop {
        match event_loop.poll().await {
            Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                log::info!("MQTT connected, subscribing to command topics");
                if let Err(e) = cmd_client.subscribe("esmart/+/set", rumqttc::QoS::AtLeastOnce).await {
                    log::error!("Failed to subscribe to command topics: {:?}", e);
                } else {
                    log::info!("Subscribed to esmart/+/set for HA commands");
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
                log::info!("Received MQTT command: topic={} payload={}", topic, payload);
                match commands::parse_mqtt_command(&jid, &topic, &payload, &actuator_states_cmd) {
                    commands::ParseResult::Command(cmd) => {
                        if let Err(e) = cmd_sender.send(cmd).await {
                            log::error!("Failed to send command to XMPP task: {:?}", e);
                        }
                    }
                    commands::ParseResult::Handled => {
                        // Publish updated travel time state back to MQTT so HA reflects the value
                        if let Err(e) = cmd_client.publish(
                            topic.replace("/set", "/state"),
                            rumqttc::QoS::AtLeastOnce,
                            true,
                            payload.trim().to_string(),
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

    // If we exit the event loop, abort the receiver task and return
    receiver_handle.abort();
    log::warn!("MQTT event loop exited, task will return for reconnect.");
}

#[tokio::main]
async fn main() {
    let args = Arc::new(cli::Cli::parse());

    env_logger::init();

    let (sender, receiver) = broadcast::channel(256);

    let messages_sent = Arc::new(AtomicU64::new(0));
    let messages_recv = Arc::new(AtomicU64::new(0));

    let ms = messages_sent.clone();
    let mr = messages_recv.clone();
    // task to report statistics every now and then
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            log::info!(
                "Messages: {} received / {} sent",
                mr.load(std::sync::atomic::Ordering::Acquire),
                ms.load(std::sync::atomic::Ordering::Acquire)
            );
        }
    });

    let args_clone = args.clone();
    let receiver_clone = receiver.resubscribe();
    let messages_sent_clone = messages_sent.clone();

    // Command channel: MQTT task sends commands, XMPP task receives and forwards to eSmart
    let (cmd_sender, cmd_receiver) = mpsc::channel::<commands::ESmartCommand>(64);

    // Spawn XMPP task
    let xmpp_handle = tokio::spawn(async move {
        let mut cmd_rx = cmd_receiver;
        loop {
            log::info!("Starting XMPP task");
            xmpp_task(sender.clone(), &mut cmd_rx, args_clone.clone(), messages_recv.clone())
                .await
                .unwrap_or_else(|e| {
                    log::error!("XMPP task failed: {:?}", e);
                });
            // Wait 5s before trying to reconnect
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    // Spawn MQTT task with reconnect loop
    let mqtt_handle = tokio::spawn(async move {
        loop {
            log::info!("Starting MQTT task");
            mqtt_task(
                receiver_clone.resubscribe(),
                cmd_sender.clone(),
                args.clone(),
                messages_sent_clone.clone(),
            )
            .await;
            log::warn!("MQTT task failed or disconnected, reconnecting in 5s...");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = xmpp_handle => {
            log::warn!("XMPP task exited, shutting down.");
        }
        _ = mqtt_handle => {
            log::warn!("MQTT task exited, shutting down.");
        }
    }
}
