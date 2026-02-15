#[derive(Clone)]
pub enum Unit {
    None,
    C,
    Lmin,
    W,
    Percent,
}

#[derive(Clone, Debug)]
pub enum StatValue {
    Float(f32),
    Text(String),
}

#[derive(Clone)]
pub enum DeviceClass {
    None,
    Water,
    Power,
    Temperature,
}

#[derive(Clone)]
pub struct Stat {
    name: String,
    unit: Unit,
    device_class: DeviceClass,
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
        Self {
            name: name.into(),
            unit,
            device_class,
            icon: icon.into(),
            property: property.into(),
        }
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
            DeviceClass::Power => Some("power"),
            DeviceClass::Temperature => Some("temperature"),
        }
    }

    pub fn unit_str(&self) -> Option<&str> {
        match self.unit {
            Unit::None => None,
            Unit::C => Some("°C"),
            Unit::Lmin => Some("L/min"),
            Unit::W => Some("W"),
            Unit::Percent => Some("%"),
        }
    }
}

pub trait IterStats<I: IntoIterator> {
    fn into_stats_iter(self) -> I;
}
