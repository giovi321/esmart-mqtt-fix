//! Cumulative energy and water totals.
//!
//! eSmart only reports instantaneous values: power in W and water flow in
//! L/min. The Home Assistant Energy dashboard needs ever-increasing totals
//! (kWh, L), so we integrate the instantaneous readings over time (trapezoidal
//! rule) and persist the result, together with the actuator travel times, to
//! a small JSON state file so both survive restarts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::data::Meter;
use crate::stats::{DeviceClass, Stat, StateClass, StatValue, Unit};

/// Gaps longer than this are not integrated: after an outage we do not know
/// what happened in between, so we skip the gap rather than guess.
const MAX_GAP_SECS: f64 = 120.0;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Accumulator {
    total: f64,
    #[serde(skip)]
    last: Option<(DateTime<Utc>, f64)>,
}

impl Accumulator {
    fn new() -> Self {
        Self { total: 0.0, last: None }
    }

    /// Add the area under the curve since the previous sample.
    /// `per_sec` converts one unit of `value` × one second into the total's unit.
    fn add(&mut self, now: DateTime<Utc>, value: f64, per_sec: f64) {
        let value = value.max(0.0);
        if let Some((ts, prev)) = self.last {
            let dt = (now - ts).num_milliseconds() as f64 / 1000.0;
            if dt > 0.0 && dt <= MAX_GAP_SECS {
                self.total += (prev + value) / 2.0 * dt * per_sec;
            }
        }
        self.last = Some((now, value));
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct PersistedState {
    /// Keyed by entity id, e.g. "energy_1" or "water_4".
    #[serde(default)]
    totals: HashMap<String, Accumulator>,
    /// Actuator travel times in seconds, keyed by "<node_id>_open" / "<node_id>_close".
    #[serde(default)]
    pub travel_times: HashMap<String, f64>,
}

pub struct Totals {
    state: PersistedState,
    path: Option<PathBuf>,
    dirty: bool,
}

pub type SharedTotals = Arc<Mutex<Totals>>;

impl Totals {
    pub fn load(path: Option<&Path>) -> Self {
        let state = match path {
            Some(p) if p.exists() => match std::fs::read_to_string(p)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
            {
                Ok(state) => {
                    log::info!("Loaded persisted state from {}", p.display());
                    state
                }
                Err(e) => {
                    // Keep the broken file for inspection rather than overwrite it
                    let backup = p.with_extension("json.broken");
                    let _ = std::fs::rename(p, &backup);
                    log::error!(
                        "Could not read state file {} ({}), moved it to {} and starting from zero",
                        p.display(),
                        e,
                        backup.display()
                    );
                    PersistedState::default()
                }
            },
            Some(p) => {
                log::info!("No state file at {}, starting totals from zero", p.display());
                PersistedState::default()
            }
            None => {
                log::warn!("No --state-file given: energy/water totals restart from zero on every restart");
                PersistedState::default()
            }
        };
        Self {
            state,
            path: path.map(|p| p.to_path_buf()),
            dirty: false,
        }
    }

    pub fn shared(self) -> SharedTotals {
        Arc::new(Mutex::new(self))
    }

    /// Feed one meter reading; returns the cumulative stat to publish, if any.
    pub fn process_meter(&mut self, now: DateTime<Utc>, meter: &Meter) -> Option<(String, Stat, StatValue)> {
        let (key, value, per_sec, stat) = match meter {
            Meter::Power { id, name, power } => (
                format!("energy_{id}"),
                *power,
                // W·s → kWh
                1.0 / 3_600_000.0,
                Stat::new(
                    &format!("{name} Energy"),
                    Unit::KWh,
                    DeviceClass::Energy,
                    "mdi:counter",
                    "energy",
                ),
            ),
            Meter::Flow { id, name, flow_rate } => (
                format!("water_{id}"),
                *flow_rate,
                // L/min · s → L
                1.0 / 60.0,
                Stat::new(
                    &format!("{name} Volume"),
                    Unit::L,
                    DeviceClass::Water,
                    "mdi:water",
                    "volume",
                ),
            ),
            _ => return None,
        };
        let acc = self
            .state
            .totals
            .entry(key.clone())
            .or_insert_with(Accumulator::new);
        let before = acc.total;
        acc.add(now, value, per_sec);
        if acc.total != before {
            self.dirty = true;
        }
        let total = acc.total;
        Some((
            key,
            stat.with_state_class(StateClass::TotalIncreasing),
            // Round so HA does not record float noise as changes
            StatValue::Float((total * 1000.0).round() / 1000.0),
        ))
    }

    pub fn travel_time(&self, node_id: u16, dir: &str) -> Option<f64> {
        self.state.travel_times.get(&format!("{node_id}_{dir}")).copied()
    }

    pub fn set_travel_time(&mut self, node_id: u16, dir: &str, secs: f64) {
        self.state.travel_times.insert(format!("{node_id}_{dir}"), secs);
        self.dirty = true;
    }

    /// Write the state file if anything changed. Writes to a temp file and
    /// renames it, so a crash mid-write never leaves a truncated file.
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        let result = serde_json::to_string_pretty(&self.state)
            .map_err(|e| e.to_string())
            .and_then(|s| std::fs::write(&tmp, s).map_err(|e| e.to_string()))
            .and_then(|_| std::fs::rename(&tmp, path).map_err(|e| e.to_string()));
        match result {
            Ok(_) => {
                self.dirty = false;
                log::debug!("Saved state to {}", path.display());
            }
            Err(e) => log::error!("Failed to save state to {}: {}", path.display(), e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn integrates_power_to_kwh() {
        let mut t = Totals::load(None);
        let m = Meter::Power { id: 1, name: "x".into(), power: 1000.0 };
        let t0 = Utc::now();
        t.process_meter(t0, &m);
        // 1 kW for one hour in 60 s steps
        let mut last = None;
        for i in 1..=60 {
            last = t.process_meter(t0 + Duration::seconds(60 * i), &m);
        }
        match last.unwrap().2 {
            StatValue::Float(v) => assert!((v - 1.0).abs() < 1e-6, "{v}"),
            _ => panic!(),
        }
    }

    #[test]
    fn integrates_flow_to_litres_and_skips_gaps() {
        let mut t = Totals::load(None);
        let m = Meter::Flow { id: 4, name: "x".into(), flow_rate: 6.0 };
        let t0 = Utc::now();
        t.process_meter(t0, &m);
        t.process_meter(t0 + Duration::seconds(60), &m); // +6 L
        let r = t.process_meter(t0 + Duration::seconds(60 + 3600), &m); // gap, skipped
        match r.unwrap().2 {
            StatValue::Float(v) => assert!((v - 6.0).abs() < 1e-6, "{v}"),
            _ => panic!(),
        }
    }
}
