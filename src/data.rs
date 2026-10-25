use crate::stats::{DeviceClass, IterStats, Stat, StatValue, Unit};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod date_format {
    use chrono::{DateTime, NaiveDateTime, Utc};
    use serde::{self, Deserialize, Deserializer, Serializer};

    const FORMAT: &str = "%Y-%m-%d %H:%M:%S%z";
    const FORMAT_ISO: &str = "%Y-%m-%dT%H:%M:%S%z";
    const FORMAT_ISO_NAIVE: &str = "%Y-%m-%dT%H:%M:%S";

    #[allow(dead_code)]
    pub fn serialize<S>(date: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = format!("{}", date.format(FORMAT));
        serializer.serialize_str(&s)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;

        if let Ok(dt) = DateTime::parse_from_str(&s, FORMAT) {
            return Ok(dt.into());
        }

        let with_tz = format!("{}+0000", s);
        if let Ok(dt) = DateTime::parse_from_str(&with_tz, FORMAT) {
            return Ok(dt.into());
        }

        if let Ok(dt) = DateTime::parse_from_str(&s, FORMAT_ISO) {
            return Ok(dt.into());
        }

        if let Ok(dt) = NaiveDateTime::parse_from_str(&s, FORMAT_ISO_NAIVE) {
            return Ok(dt.and_utc());
        }

        Err(serde::de::Error::custom(format!(
            "failed to parse timestamp: '{}'",
            s
        )))
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
pub enum OnOff {
    #[serde(rename = "on")]
    On,
    #[serde(rename = "off")]
    #[default]
    Off,
}

impl From<&OnOff> for bool {
    fn from(value: &OnOff) -> bool {
        match value {
            OnOff::On => true,
            OnOff::Off => false,
        }
    }
}

impl Serialize for OnOff {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(self.into())
    }
}

#[derive(Deserialize, Debug)]
struct HolidayMode {
    #[serde(default)]
    onoff: OnOff,
}

impl Default for HolidayMode {
    fn default() -> Self {
        Self { onoff: OnOff::Off }
    }
}

#[derive(Deserialize, Debug)]
struct Freecooling {
    #[serde(default)]
    onoff: OnOff,
}

impl Default for Freecooling {
    fn default() -> Self {
        Self { onoff: OnOff::Off }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Meter {
    Flow {
        id: u16,
        name: String,
        flow_rate: f64,
    },
    Power {
        id: u16,
        name: String,
        power: f64,
    },
    Temperature {
        id: u16,
        temperature: f64,
    },
    /// Any meter shape not known yet. Kept so that one new meter type does not
    /// make the whole message fail to deserialize.
    Unknown(serde_json::Value),
}

/// Turn a free-form name into something usable in an entity id.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Publish every scalar field of an unrecognised meter or node as a generic
/// sensor, so newly discovered eSmart features show up in Home Assistant
/// without a code change. `prefix` is "meter" or "node".
pub fn generic_stats(prefix: &str, v: &serde_json::Value) -> Vec<(String, Stat, StatValue)> {
    let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
        return vec![];
    };
    let label = v
        .get("name")
        .and_then(|n| n.as_str())
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("{} {}", prefix, id));
    // Nodes carry their values under "data", meters at top level
    let fields = match v.get("data").and_then(|d| d.as_object()) {
        Some(data) => data.clone(),
        None => v.as_object().cloned().unwrap_or_default(),
    };
    let mut stats = vec![];
    for (key, value) in fields {
        if key == "id" || key == "name" {
            continue;
        }
        let stat_value = match &value {
            serde_json::Value::Number(n) => match n.as_f64() {
                Some(f) => StatValue::Float(f),
                None => continue,
            },
            serde_json::Value::String(s) => StatValue::Text(s.clone()),
            serde_json::Value::Bool(b) => StatValue::Text(b.to_string()),
            _ => continue,
        };
        let entity_id = format!("{}_{}_{}", prefix, id, slug(&key));
        stats.push((
            entity_id,
            Stat::new(
                &format!("{} {}", label, key),
                Unit::None,
                DeviceClass::None,
                "mdi:help-circle-outline",
                &key,
            ),
            stat_value,
        ));
    }
    stats
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ValveData {
    onoff: OnOff,
    power: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RoomData {
    #[serde(rename = "deviceOnOff")]
    device_on_off: OnOff,
    power: f64,
    setpoint: f64,
    temperature: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ActuatorData {
    position: f64,
    orientation: f64,
    power: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FanData {
    speed: String,
    power: f64,
    mode: String,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Node {
    Room { id: u16, data: RoomData },
    Valve { id: u16, data: ValveData },
    Actuator { id: u16, data: ActuatorData },
    Fan { id: u16, data: FanData },
    Unknown(serde_json::Value),
}

#[derive(Deserialize, Debug)]
struct Body {
    #[serde(default)]
    holiday_mode: HolidayMode,
    #[serde(default)]
    freecooling: Freecooling,
    #[serde(default)]
    valves_state: String,
    #[serde(default)]
    modbus_on_error: String,
    #[serde(default)]
    modbus_belimo_on_error: String,
    #[serde(default)]
    modbus_ac_on_error: String,
    meters: Vec<Meter>,
    nodes: Vec<Node>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct Headers {
    from: String,
    method: String,
    size: usize,
    #[serde(with = "date_format")]
    timestamp: DateTime<Utc>,
    to: String,
    #[serde(rename = "type")]
    _type: String,
    version: String,
}

#[derive(Deserialize, Debug)]
pub struct ESmartMessage {
    #[allow(dead_code)]
    headers: Headers,
    body: Body,
}

impl ESmartMessage {
    pub fn iter_meters(&self) -> std::slice::Iter<'_, Meter> {
        self.body.meters.iter()
    }

    pub fn iter_nodes(&self) -> std::slice::Iter<'_, Node> {
        self.body.nodes.iter()
    }

    pub fn holiday_mode_on(&self) -> bool {
        (&self.body.holiday_mode.onoff).into()
    }

    pub fn freecooling_on(&self) -> bool {
        (&self.body.freecooling.onoff).into()
    }

    pub fn valves_state(&self) -> &str {
        &self.body.valves_state
    }

    pub fn modbus_on_error(&self) -> &str {
        &self.body.modbus_on_error
    }

    pub fn modbus_belimo_on_error(&self) -> &str {
        &self.body.modbus_belimo_on_error
    }

    pub fn modbus_ac_on_error(&self) -> &str {
        &self.body.modbus_ac_on_error
    }

    pub fn body_stats(&self) -> Vec<(String, Stat, StatValue)> {
        vec![
            (
                "holiday_mode".to_string(),
                Stat::new(
                    "Holiday Mode",
                    Unit::None,
                    DeviceClass::None,
                    "mdi:beach",
                    "holiday_mode",
                ),
                StatValue::Float(if self.holiday_mode_on() { 1.0 } else { 0.0 }),
            ),
            (
                "freecooling".to_string(),
                Stat::new(
                    "Freecooling",
                    Unit::None,
                    DeviceClass::None,
                    "mdi:snowflake",
                    "freecooling",
                ),
                StatValue::Float(if self.freecooling_on() { 1.0 } else { 0.0 }),
            ),
            (
                "valves_state".to_string(),
                Stat::new(
                    "Valves State",
                    Unit::None,
                    DeviceClass::None,
                    "mdi:valve",
                    "valves_state",
                ),
                StatValue::Text(self.valves_state().to_string()),
            ),
            (
                "modbus_error".to_string(),
                Stat::new(
                    "Modbus Error",
                    Unit::None,
                    DeviceClass::None,
                    "mdi:alert-circle",
                    "modbus_error",
                ),
                StatValue::Text(if self.modbus_on_error().is_empty() {
                    "OK".to_string()
                } else {
                    self.modbus_on_error().to_string()
                }),
            ),
            (
                "modbus_belimo_error".to_string(),
                Stat::new(
                    "Modbus Belimo Error",
                    Unit::None,
                    DeviceClass::None,
                    "mdi:alert-circle",
                    "modbus_belimo_error",
                ),
                StatValue::Text(if self.modbus_belimo_on_error().is_empty() {
                    "OK".to_string()
                } else {
                    self.modbus_belimo_on_error().to_string()
                }),
            ),
            (
                "modbus_ac_error".to_string(),
                Stat::new(
                    "Modbus AC Error",
                    Unit::None,
                    DeviceClass::None,
                    "mdi:alert-circle",
                    "modbus_ac_error",
                ),
                StatValue::Text(if self.modbus_ac_on_error().is_empty() {
                    "OK".to_string()
                } else {
                    self.modbus_ac_on_error().to_string()
                }),
            ),
        ]
    }
}

impl IterStats<std::vec::IntoIter<(String, Stat, StatValue)>> for &Meter {
    fn into_stats_iter(self) -> std::vec::IntoIter<(String, Stat, StatValue)> {
        match self {
            Meter::Flow {
                id,
                name,
                flow_rate,
            } => vec![(
                format!("flow_{id}"),
                Stat::new(
                    name,
                    Unit::Lmin,
                    DeviceClass::VolumeFlowRate,
                    "mdi:pipe",
                    "flow_rate",
                ),
                StatValue::Float(*flow_rate),
            )],
            Meter::Power { id, name, power } => {
                vec![(
                    format!("power_{id}"),
                    Stat::new(
                        name,
                        Unit::W,
                        DeviceClass::Power,
                        "mdi:lightning-bolt",
                        "power",
                    ),
                    StatValue::Float(*power),
                )]
            }
            Meter::Temperature { id, temperature } => vec![(
                format!("temperature_{id}"),
                Stat::new(
                    "General Temperature",
                    Unit::C,
                    DeviceClass::Temperature,
                    "mdi:thermometer",
                    "temperature",
                ),
                StatValue::Float(*temperature),
            )],
            Meter::Unknown(v) => {
                log::debug!("Unknown meter type: {:?}", v);
                generic_stats("meter", v)
            }
        }
        .into_iter()
    }
}

impl IterStats<std::vec::IntoIter<(String, Stat, StatValue)>> for &Node {
    fn into_stats_iter(self) -> std::vec::IntoIter<(String, Stat, StatValue)> {
        match self {
            Node::Room {
                id,
                data:
                    RoomData {
                        temperature,
                        device_on_off,
                        power,
                        setpoint,
                    },
            } => vec![
                (
                    format!("room_temperature_{id}"),
                    Stat::new(
                        &format!("Room {id} Temperature"),
                        Unit::C,
                        DeviceClass::Temperature,
                        "mdi:thermometer",
                        "temperature",
                    ),
                    StatValue::Float(*temperature),
                ),
                (
                    format!("room_setpoint_{id}"),
                    Stat::new(
                        &format!("Room {id} Setpoint"),
                        Unit::C,
                        DeviceClass::Temperature,
                        "mdi:thermometer-chevron-up",
                        "setpoint",
                    ),
                    StatValue::Float(*setpoint),
                ),
                (
                    format!("room_heating_on_{id}"),
                    Stat::new(
                        &format!("Room {id} Heating On"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:radiator",
                        "deviceOnOff",
                    ),
                    StatValue::Float(if device_on_off.into() { 1.0 } else { 0.0 }),
                ),
                (
                    format!("room_heating_power_{id}"),
                    Stat::new(
                        &format!("Room {id} Heating Power"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:radiator",
                        "power",
                    ),
                    StatValue::Float(*power),
                ),
            ]
            .into_iter(),
            Node::Valve {
                id,
                data: ValveData { power, onoff },
            } => vec![
                (
                    format!("valve_power_{id}"),
                    Stat::new(
                        &format!("Valve {id} Power"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:valve",
                        "power",
                    ),
                    StatValue::Float(*power),
                ),
                (
                    format!("valve_on_{id}"),
                    Stat::new(
                        &format!("Valve {id} On"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:valve",
                        "onoff",
                    ),
                    StatValue::Float(if onoff.into() { 1.0 } else { 0.0 }),
                ),
            ]
            .into_iter(),
            Node::Actuator {
                id,
                data: ActuatorData { position, orientation, power },
            } => {
                // eSmart uses 0-1024: 0=fully open, 1024=fully closed
                // HA uses 0-100: 0=closed, 100=open, so we invert and scale
                let ha_position = ((1024.0 - position) / 1024.0 * 100.0).round();
                let ha_orientation = ((1024.0 - orientation) / 1024.0 * 100.0).round();
                vec![
                (
                    format!("actuator_position_{id}"),
                    Stat::new(
                        &format!("Actuator {id} Position"),
                        Unit::Percent,
                        DeviceClass::None,
                        "mdi:window-shutter",
                        "position",
                    ),
                    StatValue::Float(ha_position),
                ),
                (
                    format!("actuator_orientation_{id}"),
                    Stat::new(
                        &format!("Actuator {id} Orientation"),
                        Unit::Percent,
                        DeviceClass::None,
                        "mdi:rotate-3d-variant",
                        "orientation",
                    ),
                    StatValue::Float(ha_orientation),
                ),
                (
                    format!("actuator_power_{id}"),
                    Stat::new(
                        &format!("Actuator {id} Power"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:window-shutter",
                        "power",
                    ),
                    StatValue::Float(*power),
                ),
            ]
            }.into_iter(),
            Node::Fan {
                id,
                data: FanData { speed, power, mode },
            } => vec![
                (
                    format!("fan_speed_{id}"),
                    Stat::new(
                        &format!("Fan {id} Speed"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:fan",
                        "speed",
                    ),
                    StatValue::Text(speed.clone()),
                ),
                (
                    format!("fan_mode_{id}"),
                    Stat::new(
                        &format!("Fan {id} Mode"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:fan",
                        "mode",
                    ),
                    StatValue::Text(mode.clone()),
                ),
                (
                    format!("fan_power_{id}"),
                    Stat::new(
                        &format!("Fan {id} Power"),
                        Unit::None,
                        DeviceClass::None,
                        "mdi:fan",
                        "power",
                    ),
                    StatValue::Float(*power),
                ),
            ]
            .into_iter(),
            Node::Unknown(v) => {
                log::debug!("Unknown node type: {:?}", v);
                generic_stats("node", v).into_iter()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_meter_does_not_break_message() {
        let body = r#"{"headers":{"from":"master","method":"INFO","size":1,"timestamp":"2026-10-25 12:51:28+0000","to":"remotes","type":"data","version":"1.19.0"},
            "body":{"meters":[{"id":4,"flow_rate":1.5,"name":"Cold water"},{"id":30,"volume":12.5,"name":"New meter"}],
            "nodes":[{"id":337,"data":{"speed":"33%","power":0,"mode":"manual"}},{"id":400,"data":{"foo":1}}]}}"#;
        let msg: ESmartMessage = serde_json::from_str(body).unwrap();
        let meter_ids: Vec<String> = msg.iter_meters().flat_map(|m| m.into_stats_iter()).map(|s| s.0).collect();
        assert_eq!(meter_ids, vec!["flow_4", "meter_30_volume"]);
        let node_ids: Vec<String> = msg.iter_nodes().flat_map(|n| n.into_stats_iter()).map(|s| s.0).collect();
        assert!(node_ids.contains(&"fan_speed_337".to_string()));
        assert!(node_ids.contains(&"node_400_foo".to_string()));
    }

    #[test]
    fn flow_uses_volume_flow_rate_class() {
        let m = Meter::Flow { id: 3, name: "Hot Water".into(), flow_rate: 2.0 };
        let (_, stat, _) = (&m).into_stats_iter().next().unwrap();
        assert_eq!(stat.device_class_str(), Some("volume_flow_rate"));
        assert_eq!(stat.unit_str(), Some("L/min"));
    }
}
