# eSmart - MQTT Bridge

## Features
* Reads data from eSmart through their XMPP API (you'll need a rooted phone to get the password!)
* Writes to an MQTT broker
* **Bidirectional control** - Send commands from Home Assistant to control your eSmart devices
* Home Assistant auto-discovery for sensors and control entities
* Built-in throttling to avoid spamming the broker
* MQTT authentication support
* Written in Rust 🦀

## Supported Entities

### Sensors (Read-only)
* **Energy meters**: Power consumption, energy usage
* **Water meters**: Flow rate, consumption
* **Room sensors**: Temperature, humidity, heating power
* **Actuator sensors**: Position, orientation
* **Fan sensors**: Power, speed, mode
* **System sensors**: Freecooling status, holiday mode, valve states, Modbus errors

### Control Entities (Write-enabled)
* **Climate entities** (per room): Control heating on/off and temperature setpoint
* **Cover entities** (per actuator): Control position (0-100%) and orientation/tilt
* **Fan entities** (per fan): Control on/off, speed percentage, and mode (auto/manual)
* **Switch entities**: Holiday mode and freecooling on/off

## History

The eSmart is an all-in-one domotics solution made in Switzerland, which lets you regulate heating, track energy/water consumption and works as a doorbell and intercom, among others.
Unfortunately, as of now, it doesn't provide an open API which can integrate with other systems. I did contact them on that, to no avail. So, I took it as my mission to reverse-engineer
their API and provide an MQTT-compatible bridge. Fortunately, there was already some information online (see "Acknowledgements"), which made things easier.

## Running

The following values have to be obtained from the eSmart app, using a rooted phone (see following section for instructions on how to do it):

| CLI argument   | Env. var  | Meaning | Default |
| -------------- |-----------|---------|---------|
| --xmpp-jid     | XMPP_JID  | JID + `@myesmart.net` ||
| --xmpp-room    | XMPP_ROOM | DEVICE_ID + `@conference.myesmart.net` ||
| --xmpp-password | XMPP_PASSWORD | The XMPP password used by the mobile app ||
| --xmpp-nickname | XMPP_NICKNAME | Nickname to use in the XMPP room (arbitrary AFAIK) | `esmart` |
| --mqtt-hostname | MQTT_HOSTNAME | Host name of your MQTT broker ||
| --mqtt-port     | MQTT_PORT | Port where your broker is running | 8113 |
| --mqtt-id       | MQTT_ID   | ID used to identify the device with the MQTT broker | `esmarter` |
| --mqtt-username | MQTT_USERNAME | MQTT username (optional) | |
| --mqtt-password | MQTT_PASSWORD | MQTT password (optional) | |
| --mqtt-throttling-secs | MQTT_THROTTLING_SECS | how many seconds to keep between MQTT updates. values which are received between updates are averaged across this period | 300

The ideal way to run the program is probably by placing the configuration settings in environment variables, but you can also pass arguments through the CLI, e.g.:

```sh
$ esmart_mqtt --xmpp-nickname="the_watcher"
```

### Usage Examples

#### Basic Usage with Environment Variables
```sh
export XMPP_JID="your_jid@myesmart.net"
export XMPP_PASSWORD="your_password"
export XMPP_ROOM="your_device_id@conference.myesmart.net"
export MQTT_HOSTNAME="localhost"
export MQTT_USERNAME="mqtt_user"
export MQTT_PASSWORD="mqtt_pass"

$ esmart_mqtt
```

#### Docker Usage
```sh
docker run -d \
  -e XMPP_JID="your_jid@myesmart.net" \
  -e XMPP_PASSWORD="your_password" \
  -e XMPP_ROOM="your_device_id@conference.myesmart.net" \
  -e MQTT_HOSTNAME="mqtt-broker" \
  -e MQTT_USERNAME="mqtt_user" \
  -e MQTT_PASSWORD="mqtt_pass" \
  --name esmart-mqtt \
  esmart-mqtt:latest
```

### Home Assistant Integration

Once running, the bridge will automatically:
1. **Publish sensor data** to topics like `esmart/room_temperature_1/state`
2. **Send discovery messages** to `homeassistant/sensor/esmart/.../config`
3. **Create control entities** for climate, covers, fans, and switches
4. **Subscribe to command topics** like `esmart/+/set`

#### Control Examples

**Set room temperature:**
```bash
# MQTT topic: esmart/room_1_setpoint/set
# Payload: 21.5
mosquitto_pub -h localhost -t "esmart/room_1_setpoint/set" -m "21.5"
```

**Control actuator (window shutter):**
```bash
# Set position to 50%
mosquitto_pub -h localhost -t "esmart/actuator_1_position/set" -m "50"

# Set tilt/orientation to 75%
mosquitto_pub -h localhost -t "esmart/actuator_1_tilt/set" -m "75"
```

**Control fan:**
```bash
# Turn fan on
mosquitto_pub -h localhost -t "esmart/fan_1_onoff/set" -m "ON"

# Set fan speed to 75%
mosquitto_pub -h localhost -t "esmart/fan_1_speed/set" -m "75%"

# Set fan mode to auto
mosquitto_pub -h localhost -t "esmart/fan_1_mode/set" -m "auto"
```

**Toggle holiday mode:**
```bash
mosquitto_pub -h localhost -t "esmart/holiday_mode/set" -m "ON"
```

**Toggle freecooling:**
```bash
mosquitto_pub -h localhost -t "esmart/freecooling/set" -m "ON"
```

The help option (`-h`) describes each option quite well.

Builds are provided for `x86_64`, `armv7` and `aarch64`, but any architecture/OS supported by the underlying crates should be OK.

### Getting the JID/password (rooted phone needed!)

First of all, get a **rooted Android phone**, then install the eSmart App from the Play Store. Then, follow the usual app onboarding process to link your phone to the eSmart. Once that's done, use ADB to read the authentication parameters from the phone:

```sh
$ adb root
$ adb shell
OnePlus5T:/ # cat /data/data/ch.myesmart.esmartlive/shared_prefs/prefs_secure_info.xml
```

You should get output which resembles this:

```xml
<?xml version='1.0' encoding='utf-8' standalone='yes' ?>
<map>
    <string name="prefs_connected_devices">[{&quot;id&quot;:&quot;xxxxxxxxxxxxxx&quot;,&quot;isEnable&quot;:true,&quot;name&quot;:&quot;xxxxxxxxxxxxxxxxxxxx&quot;}]</string>
    <string name="prefs_secure_hash">[this is your PASSWORD]</string>
    <string name="prefs_jid">[this is your JID, without the `@myesmart.net` suffix]</string>
    <string name="prefs_selected_device">[this is the DEVICE_ID. It can be used to generate the XMPP_ROOM, by adding the suffix `@conference.myesmart.net`]</string>
</map>
```

## Building from source

```sh
$ cargo build --release
```

## TODO

* [ ] Reverse-engineering the app registration process (no need for a rooted phone anymore?)
* [ ] Add more comprehensive error handling and retry logic
* [ ] Support for additional eSmart features as they are discovered

## Recent Changes

### Version 0.1.0+ (Current Development)

**Major Features Added:**
- **Bidirectional Control**: Full write support for controlling eSmart devices via MQTT
- **Enhanced Sensor Support**: Added all missing read-only sensors from the eSmart API
- **MQTT Authentication**: Support for username/password authentication with MQTT brokers
- **Improved Home Assistant Integration**: Automatic discovery of control entities (climate, cover, fan, switch)

**New Control Capabilities:**
- **Climate Control**: Per-room heating on/off and temperature setpoint control
- **Cover Control**: Actuator position (0-100%) and orientation/tilt control  
- **Fan Control**: On/off, speed percentage, and mode (auto/manual) control
- **System Switches**: Holiday mode and freecooling on/off control

**Technical Improvements:**
- Added `commands.rs` module for XMPP command generation
- Implemented `mpsc` channel for MQTT→XMPP command forwarding
- Added proper Home Assistant discovery for all entity types
- Fixed device icons and classes for better UI integration
- Enhanced logging for command processing and authentication

**Dependencies Updated:**
- Added `sync` and `time` features to tokio for async coordination
- Improved error handling and connection stability

## Acknowledgements

* Nils Amiet - "The smart home I didn't ask for" - https://www.youtube.com/watch?v=CE7jRpUa29k
