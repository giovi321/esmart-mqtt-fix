#[derive(Clone, Debug)]
pub enum Unit {
    None,
    C,
    Lmin,
    L,
    W,
    KWh,
    Percent,
}

#[derive(Clone, Debug)]
pub enum StatValue {
    Float(f64),
    Text(String),
}

#[derive(Clone, Debug)]
pub enum DeviceClass {
    None,
    /// Cumulative water volume (L). Valid for the HA Energy dashboard.
    Water,
    /// Instantaneous water flow (L/min).
    VolumeFlowRate,
    Power,
    Energy,
    Temperature,
}

/// Home Assistant `state_class`.
#[derive(Clone, Debug, PartialEq)]
pub enum StateClass {
    None,
    Measurement,
    TotalIncreasing,
}

#[derive(Clone, Debug)]
pub struct Stat {
    name: String,
    unit: Unit,
    device_class: DeviceClass,
    state_class: StateClass,
    icon: String,
    property: String,
}

impl Stat {
    pub fn new(
        name: &str,
        unit: Unit,
        device_class: DeviceClass,
        icon: &str,
        property: &str,
    ) -> Self {
        // Anything with a unit is a measurement unless stated otherwise
        let state_class = match unit {
            Unit::None => StateClass::None,
            _ => StateClass::Measurement,
        };
        Self {
            name: name.into(),
            unit,
            device_class,
            state_class,
            icon: icon.into(),
            property: property.into(),
        }
    }

    pub fn with_state_class(mut self, state_class: StateClass) -> Self {
        self.state_class = state_class;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn icon(&self) -> &str {
        &self.icon
    }

    pub fn property(&self) -> &str {
        &self.property
    }

    pub fn device_class_str(&self) -> Option<&str> {
        match self.device_class {
            DeviceClass::None => None,
            DeviceClass::Water => Some("water"),
            DeviceClass::VolumeFlowRate => Some("volume_flow_rate"),
            DeviceClass::Power => Some("power"),
            DeviceClass::Energy => Some("energy"),
            DeviceClass::Temperature => Some("temperature"),
        }
    }

    pub fn unit_str(&self) -> Option<&str> {
        match self.unit {
            Unit::None => None,
            Unit::C => Some("°C"),
            Unit::Lmin => Some("L/min"),
            Unit::L => Some("L"),
            Unit::W => Some("W"),
            Unit::KWh => Some("kWh"),
            Unit::Percent => Some("%"),
        }
    }

    pub fn state_class_str(&self) -> Option<&str> {
        match self.state_class {
            StateClass::None => None,
            StateClass::Measurement => Some("measurement"),
            StateClass::TotalIncreasing => Some("total_increasing"),
        }
    }
}

pub trait IterStats<I: IntoIterator> {
    fn into_stats_iter(self) -> I;
}
