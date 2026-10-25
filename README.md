# eSmart MQTT bridge

Bridges an eSmart home automation system to MQTT, with Home Assistant discovery and two-way control. It reads the eSmart XMPP feed, publishes sensors to MQTT, turns Home Assistant commands into eSmart commands, and computes cumulative energy and water totals for the Home Assistant Energy dashboard.

Fork of [pferreir/esmart-mqtt](https://github.com/pferreir/esmart-mqtt), written in Rust.

## Features

- Reads all data from the eSmart XMPP API (a rooted phone is needed once, to get the password)
- Publishes to any MQTT broker, with optional username and password
- Home Assistant MQTT discovery for sensors, climate, covers, fans, switches, binary sensors and numbers
- Two-way control: rooms, shutters, fans, holiday mode, freecooling
- Cumulative energy (kWh) and water (L) sensors usable in the Home Assistant Energy dashboard, persisted across restarts
- Availability reporting: entities go unavailable when the bridge or the eSmart feed is down
- Automatic reconnect with exponential backoff for both XMPP and MQTT
- Unknown meters and nodes are published as generic sensors, so new eSmart features appear without a code change
- Raw command topic for trying out eSmart commands the bridge does not know yet
- Per-entity throttling to limit broker traffic

## Entities

All entities are grouped under Home Assistant devices: one per room, actuator, fan and valve, plus an `eSmart System` device for meters and system state.

### Sensors

| Source | Entity id pattern | Unit | Notes |
|---|---|---|---|
| Power meters | `power_<id>` | W | Instantaneous |
| Energy totals | `energy_<id>` | kWh | One per power meter, `total_increasing`, computed by the bridge |
| Water flow meters | `flow_<id>` | L/min | Instantaneous, device class `volume_flow_rate` |
| Water totals | `water_<id>` | L | One per flow meter, `total_increasing`, computed by the bridge |
| Temperature meter | `temperature_<id>` | °C | |
| Rooms | `room_temperature_<id>`, `room_setpoint_<id>`, `room_heating_on_<id>`, `room_heating_power_<id>` | | |
| Actuators | `actuator_position_<id>`, `actuator_orientation_<id>`, `actuator_power_<id>` | % | Scaled to Home Assistant's 0 to 100, 100 is open |
| Fans | `fan_speed_<id>`, `fan_mode_<id>`, `fan_power_<id>` | | |
| Valves | `valve_power_<id>`, `valve_on_<id>` | | |
| System | `valves_state`, `modbus_error`, `modbus_belimo_error`, `modbus_ac_error` | | `OK` when empty |
| Unknown meters | `meter_<id>_<field>` | | One sensor per scalar field |
| Unknown nodes | `node_<id>_<field>` | | One sensor per scalar field, own device per node |
| Command errors | `last_command_error` | | Last error returned by eSmart, with the command that caused it |

### Controls

| Entity | Home Assistant type | Command topic(s) |
|---|---|---|
| Room | climate (off/heat, setpoint 18 to 24 °C in 1 °C steps) | `esmart/room_<id>_setpoint/set` |
| Actuator | cover with tilt | `esmart/actuator_<id>_position/set`, `esmart/actuator_<id>_tilt/set` |
| Actuator travel times | number (seconds) | `esmart/actuator_<id>_travel_open/set`, `esmart/actuator_<id>_travel_close/set` |
| Fan | fan, 3 speeds | `esmart/fan_<id>_onoff/set`, `esmart/fan_<id>_level/set`, `esmart/fan_<id>_speed/set`, `esmart/fan_<id>_mode/set` |
| Holiday mode | switch and binary sensor | `esmart/holiday_mode/set` |
| Freecooling | switch and binary sensor | `esmart/freecooling/set` |
| Raw command | none | `esmart/raw_command/set` |

## How it works

eSmart publishes an `INFO`/`data` JSON message into an XMPP multi-user chat room roughly every one to two seconds. The message carries all meters, all nodes (rooms, actuators, fans, valves) and the system state. The bridge joins that room, parses each message and publishes one MQTT state topic per entity: `esmart/<entity_id>/state`, with a JSON payload such as `{"power": 466.0}`.

Commands are `CMD` messages sent to the same room: `operation` for nodes (`{"id": 337, "speed": "33%", "mode": "manual"}`) and `tablet_operation` for holiday mode and freecooling. eSmart answers an invalid command with an `INFO` message carrying `error.message`, for example `invalid values`. The bridge logs it and publishes it to the `last_command_error` sensor.

### Energy and water totals

eSmart only reports instantaneous power (W) and flow (L/min). The Home Assistant Energy dashboard needs cumulative totals, so the bridge integrates every reading over time with the trapezoidal rule and publishes `energy_<id>` (kWh) and `water_<id>` (L) as retained `total_increasing` sensors.

Limits of this approach:

- Consumption while the bridge is not running, or during a gap of more than 120 seconds between two messages, is not counted. The totals are therefore a lower bound of what the physical meters show
- Totals are only as accurate as eSmart's instantaneous readings and their sampling rate
- The flow unit is assumed to be L/min, as in the original project. It has not been checked against a physical meter reading

Totals and actuator travel times are written to the state file (`--state-file`) every 60 seconds and on shutdown. The write goes to a temporary file that is then renamed, so a crash cannot leave a truncated file. An unreadable state file is renamed to `*.json.broken` and the bridge starts from zero. Without `--state-file`, totals restart from zero at every restart; Home Assistant treats that as a meter reset and keeps its long-term statistics.

### Fans

The fan tested (node 337, a 3-speed ventilation unit) accepts only the speeds `0%`, `33%`, `66%` and `99%`, and only mode `manual`. Any other value, including `50%`, `100%` and mode `auto`, is answered with `invalid values`. It reports `power` as 0 even while running.

The bridge therefore:

- Derives the fan's on/off state and speed from `speed`, not from `power`
- Exposes the fan in Home Assistant with 3 speed steps, sent as level 1 to 3 on `esmart/fan_<id>_level/set`
- Snaps any percentage on `esmart/fan_<id>_speed/set` to the nearest accepted speed (1 to 33 gives `33%`, 34 to 66 gives `66%`, 67 to 100 gives `99%`)
- Restores the last non-zero speed on ON, or `33%` if none was seen yet

Mode `auto` can still be sent on `esmart/fan_<id>_mode/set` for installations that accept it, but it is not exposed in Home Assistant.

### Shutters and STOP

eSmart has no stop command, and reports the target position as soon as a move starts. On STOP the bridge estimates the current position from the time elapsed since the move started and the configured travel time, then sends that position. Set the open and close travel times per actuator with the two number entities. Values are kept in the state file.

### Availability and recovery

- The bridge registers an MQTT last will on `esmart/status` (`offline`, retained) and publishes `online` only while eSmart data is fresh. All discovered entities use this topic as their availability topic
- If no eSmart data arrives for `--xmpp-stale-secs` seconds, the XMPP session is treated as dead and reconnected, even if the TCP connection still looks open
- XMPP and MQTT reconnect with exponential backoff from 5 seconds to 5 minutes. The backoff resets after a session that worked
- When Home Assistant restarts (`online` on `homeassistant/status`), all discovery messages are sent again
- Commands that could not be delivered within `--command-timeout-secs` (for example during an XMPP reconnect) are dropped and logged, not replayed late
- A meter or node with an unknown shape no longer makes the whole message fail to parse

## Running

Values marked "from the app" come from the eSmart app on a rooted phone, see [Getting the JID and password](#getting-the-jid-and-password).

| CLI argument | Environment variable | Meaning | Default |
|---|---|---|---|
| `--xmpp-jid` | `XMPP_JID` | JID with the `@myesmart.net` suffix (from the app) | |
| `--xmpp-password` | `XMPP_PASSWORD` | XMPP password used by the app (from the app) | |
| `--xmpp-room` | `XMPP_ROOM` | Device id with the `@conference.myesmart.net` suffix (from the app) | |
| `--xmpp-nickname` | `XMPP_NICKNAME` | Nickname used in the XMPP room | `esmart` |
| `--xmpp-stale-secs` | `XMPP_STALE_SECS` | Reconnect XMPP after this many seconds without data | `120` |
| `--mqtt-hostname` | `MQTT_HOSTNAME` | MQTT broker host name | |
| `--mqtt-port` | `MQTT_PORT` | MQTT broker port | `8113` |
| `--mqtt-id` | `MQTT_ID` | MQTT client id | `esmarter` |
| `--mqtt-username` | `MQTT_USERNAME` | MQTT username | none |
| `--mqtt-password` | `MQTT_PASSWORD` | MQTT password, required if a username is set | none |
| `--mqtt-throttling-secs` | `MQTT_THROTTLING_SECS` | Minimum seconds between two updates of the same entity. The latest value wins | `300` |
| `--state-file` | `STATE_FILE` | JSON file for energy/water totals and travel times | none |
| `--command-timeout-secs` | `COMMAND_TIMEOUT_SECS` | Drop undelivered commands older than this | `30` |

Log level is set with `RUST_LOG`, for example `RUST_LOG=info` or `RUST_LOG=esmart_mqtt=debug`. At debug level every raw XMPP message is logged.

### systemd

Keep credentials out of the unit file, which any local user can read with `systemctl cat`. Put them in an environment file readable only by root:

```sh
install -m 600 /dev/null /etc/esmart-mqtt.env
cat > /etc/esmart-mqtt.env <<'EOF'
XMPP_JID=your_jid@myesmart.net
XMPP_PASSWORD=your_password
XMPP_ROOM=your_device_id@conference.myesmart.net
MQTT_HOSTNAME=localhost
MQTT_PORT=1883
MQTT_USERNAME=mqtt_user
MQTT_PASSWORD=mqtt_pass
MQTT_THROTTLING_SECS=5
STATE_FILE=/var/lib/esmart-mqtt/state.json
EOF
```

```ini
# /etc/systemd/system/esmart-mqtt.service
[Unit]
Description=eSmart MQTT Bridge
After=network-online.target mosquitto.service
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=/etc/esmart-mqtt.env
Environment=RUST_LOG=info
StateDirectory=esmart-mqtt
ExecStart=/usr/local/bin/esmart_mqtt
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
```

`StateDirectory=` creates `/var/lib/esmart-mqtt`. The bridge saves the state file on SIGTERM, so `systemctl stop` and `systemctl restart` lose no totals.

### Docker

```sh
docker run -d \
  -e XMPP_JID="your_jid@myesmart.net" \
  -e XMPP_PASSWORD="your_password" \
  -e XMPP_ROOM="your_device_id@conference.myesmart.net" \
  -e MQTT_HOSTNAME="mqtt-broker" \
  -e MQTT_USERNAME="mqtt_user" \
  -e MQTT_PASSWORD="mqtt_pass" \
  -e STATE_FILE="/data/state.json" \
  -v esmart-data:/data \
  --name esmart-mqtt \
  esmart-mqtt:latest
```

The `Dockerfile` downloads a release binary from this repository: build it with `--build-arg VERSION=<tag>`.

## Home Assistant

### Energy dashboard

Use the bridge's `energy_<id>` and `water_<id>` sensors, not the `power_<id>` and `flow_<id>` ones: the dashboard only accepts cumulative sensors. On an installation with the meters in the example below:

- Electricity grid consumption: `Electricity Energy` (`energy_1`)
- Water: `Cold water Volume` (`water_4`) and `Hot Water Volume` (`water_3`)

New sensors show `statistics_not_defined` in the dashboard settings until the recorder has compiled their first 5-minute statistics.

The meter list depends on the installation. One installation reports these meters, with some appearing twice under a German name:

| id | Name | Type |
|---|---|---|
| 0 | Global energy | power |
| 1 | Electricity | power |
| 2 | Heating | power |
| 3 | Hot Water | power and flow |
| 4 | Cold water | flow |
| 7 | Cooling | power |
| 9 | Cooling and Heating | power |
| 13 | (none) | temperature |
| 17 | Stromzähler1 | power |
| 18 | Heizungszähler1 | power |
| 19 | Warmwasserzähler1 | power |
| 20 | Kaltwasserzähler1 | flow |

On that installation `Global energy` equals `Electricity` plus `Heating`, and `Stromzähler1` equals `Electricity`. Add only one meter of each pair to the dashboard, or consumption is counted twice.

### Control examples

```sh
# Room setpoint (integer, 18 to 24, or MAX)
mosquitto_pub -t esmart/room_1_setpoint/set -m 21

# Shutter position (0 closed, 100 open), OPEN, CLOSE or STOP
mosquitto_pub -t esmart/actuator_9_position/set -m 50
mosquitto_pub -t esmart/actuator_9_position/set -m STOP

# Shutter tilt (0 shade, 100 light)
mosquitto_pub -t esmart/actuator_9_tilt/set -m 75

# Shutter travel times in seconds, used to estimate the position on STOP
mosquitto_pub -t esmart/actuator_9_travel_close/set -m 25

# Fan on (last speed), off, step 1 to 3, or a percentage snapped to a valid speed
mosquitto_pub -t esmart/fan_337_onoff/set -m ON
mosquitto_pub -t esmart/fan_337_onoff/set -m OFF
mosquitto_pub -t esmart/fan_337_level/set -m 2
mosquitto_pub -t esmart/fan_337_speed/set -m 60

# Holiday mode and freecooling
mosquitto_pub -t esmart/holiday_mode/set -m ON
mosquitto_pub -t esmart/freecooling/set -m OFF
```

## Discovering new eSmart features

1. Run the bridge with `RUST_LOG=esmart_mqtt=debug` and look at the `Raw XMPP message body` lines. Meters and nodes the bridge does not know are also published as `meter_<id>_<field>` and `node_<id>_<field>` sensors
2. Use the official eSmart app while the bridge runs. Commands sent by any other client in the room are logged at info level as `Message from another client`, so the exact JSON the app sends can be copied
3. Replay or vary that JSON through the raw command topic, and watch `last_command_error` for the answer:

```sh
# Node operation: the payload is the command body
mosquitto_pub -t esmart/raw_command/set -m '{"id": 337, "speed": "33%", "mode": "manual"}'

# Any other message type: give the header type and the body
mosquitto_pub -t esmart/raw_command/set -m '{"type": "tablet_operation", "body": {"type": "holiday_mode", "onoff": "off"}}'
```

Once a command is understood, add a typed topic for it in `src/commands.rs` and a test next to the existing ones.

## Getting the JID and password

You need a rooted Android phone. Install the eSmart app from the Play Store, link it to your eSmart through the normal onboarding, then read the app's preferences with ADB:

```sh
adb root
adb shell
cat /data/data/ch.myesmart.esmartlive/shared_prefs/prefs_secure_info.xml
```

The output looks like this:

```xml
<?xml version='1.0' encoding='utf-8' standalone='yes' ?>
<map>
    <string name="prefs_connected_devices">[{&quot;id&quot;:&quot;xxxxxxxxxxxxxx&quot;,&quot;isEnable&quot;:true,&quot;name&quot;:&quot;xxxxxxxxxxxxxxxxxxxx&quot;}]</string>
    <string name="prefs_secure_hash">[this is your PASSWORD]</string>
    <string name="prefs_jid">[this is your JID, without the `@myesmart.net` suffix]</string>
    <string name="prefs_selected_device">[this is the DEVICE_ID; add `@conference.myesmart.net` to get XMPP_ROOM]</string>
</map>
```

## Building from source

```sh
cargo build --release
cargo test
```

Releases are built by CI for `x86_64` (glibc and musl), `armv7` and `aarch64` when a tag is pushed.

## TODO

- Reverse-engineer the app registration process, so no rooted phone is needed
- Find out whether eSmart exposes its own cumulative meter counters (the app shows consumption history); if so, use them instead of integrating instantaneous values
- Check the flow unit against a physical water meter

## Acknowledgements

- Nils Amiet, "The smart home I didn't ask for": https://www.youtube.com/watch?v=CE7jRpUa29k
- pferreir, author of the original [esmart-mqtt](https://github.com/pferreir/esmart-mqtt)
