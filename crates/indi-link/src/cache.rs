//! Client-side mirror of all devices and properties defined by the server.

use indexmap::IndexMap;

use crate::parser::{IndiMessage, SetValue};
use crate::types::*;

#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Device {
    pub name: String,
    pub properties: IndexMap<String, Property>,
}

impl Device {
    /// Bitmask from `DRIVER_INFO.DRIVER_INTERFACE`, see [`crate::interface`].
    pub fn interfaces(&self) -> u32 {
        self.properties
            .get("DRIVER_INFO")
            .and_then(|p| p.text("DRIVER_INTERFACE"))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn has_interface(&self, flag: u32) -> bool {
        self.interfaces() & flag != 0
    }

    pub fn is_connected(&self) -> bool {
        self.properties
            .get("CONNECTION")
            .and_then(|p| p.switch("CONNECT"))
            .unwrap_or(false)
    }

    /// Group names in definition order.
    pub fn groups(&self) -> Vec<&str> {
        let mut groups: Vec<&str> = Vec::new();
        for p in self.properties.values() {
            if !groups.contains(&p.group.as_str()) {
                groups.push(&p.group);
            }
        }
        groups
    }
}

/// What a message changed in the cache.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Defined { device: String, name: String },
    Updated { device: String, name: String },
    Deleted { device: String, name: Option<String> },
    None,
}

#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Cache {
    pub devices: IndexMap<String, Device>,
}

impl Cache {
    pub fn property(&self, device: &str, name: &str) -> Option<&Property> {
        self.devices.get(device)?.properties.get(name)
    }

    pub fn clear(&mut self) {
        self.devices.clear();
    }

    pub fn apply(&mut self, msg: &IndiMessage) -> Change {
        match msg {
            IndiMessage::Def { property, .. } => {
                let device = self
                    .devices
                    .entry(property.device.clone())
                    .or_insert_with(|| Device {
                        name: property.device.clone(),
                        properties: IndexMap::new(),
                    });
                device
                    .properties
                    .insert(property.name.clone(), property.clone());
                Change::Defined {
                    device: property.device.clone(),
                    name: property.name.clone(),
                }
            }
            IndiMessage::Set(set) => {
                let Some(prop) = self
                    .devices
                    .get_mut(&set.device)
                    .and_then(|d| d.properties.get_mut(&set.name))
                else {
                    return Change::None;
                };
                if prop.kind != set.kind {
                    return Change::None;
                }
                if let Some(state) = set.state {
                    prop.state = state;
                }
                if let Some(timeout) = set.timeout {
                    prop.timeout = timeout;
                }
                if set.timestamp.is_some() {
                    prop.timestamp = set.timestamp.clone();
                }
                for update in &set.elements {
                    let Some(el) = prop.elements.iter_mut().find(|e| e.name == update.name)
                    else {
                        continue;
                    };
                    match (&mut el.value, &update.value) {
                        (ElementValue::Text(v), SetValue::Text(n)) => v.clone_from(n),
                        (
                            ElementValue::Number(v),
                            SetValue::Number {
                                value,
                                min,
                                max,
                                step,
                            },
                        ) => {
                            v.value = *value;
                            v.min = min.unwrap_or(v.min);
                            v.max = max.unwrap_or(v.max);
                            v.step = step.unwrap_or(v.step);
                        }
                        (ElementValue::Switch(v), SetValue::Switch(n)) => *v = *n,
                        (ElementValue::Light(v), SetValue::Light(n)) => *v = *n,
                        (ElementValue::Blob(v), SetValue::Blob(n)) => *v = n.clone(),
                        _ => {}
                    }
                }
                Change::Updated {
                    device: set.device.clone(),
                    name: set.name.clone(),
                }
            }
            IndiMessage::Del { device, name, .. } => {
                match name {
                    Some(name) => {
                        if let Some(d) = self.devices.get_mut(device) {
                            d.properties.shift_remove(name);
                        }
                    }
                    None => {
                        self.devices.shift_remove(device);
                    }
                }
                Change::Deleted {
                    device: device.clone(),
                    name: name.clone(),
                }
            }
            IndiMessage::Message { .. } => Change::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;
    use quick_xml::Reader;
    use quick_xml::events::Event;

    fn apply_xml(cache: &mut Cache, xml: &str) -> Vec<Change> {
        let mut reader = Reader::from_str(xml);
        let mut parser = Parser::new();
        let mut changes = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Eof => break,
                ev => {
                    if let Some(Ok(msg)) = parser.feed(ev) {
                        changes.push(cache.apply(&msg));
                    }
                }
            }
        }
        changes
    }

    #[test]
    fn define_update_delete() {
        let mut cache = Cache::default();
        apply_xml(&mut cache, r#"
<defSwitchVector device="Cam" name="CONNECTION" group="Main" state="Idle" perm="rw" rule="OneOfMany">
  <defSwitch name="CONNECT">Off</defSwitch><defSwitch name="DISCONNECT">On</defSwitch>
</defSwitchVector>
<defTextVector device="Cam" name="DRIVER_INFO" group="General Info" perm="ro">
  <defText name="DRIVER_INTERFACE">22</defText>
</defTextVector>
<defNumberVector device="Cam" name="CCD_TEMPERATURE" group="Main" perm="rw">
  <defNumber name="CCD_TEMPERATURE_VALUE" format="%.2f" min="-50" max="50" step="0.1">20</defNumber>
</defNumberVector>"#);
        let dev = &cache.devices["Cam"];
        assert!(!dev.is_connected());
        assert!(dev.has_interface(interface::CCD));
        assert!(dev.has_interface(interface::FILTER));
        assert!(!dev.has_interface(interface::TELESCOPE));
        assert_eq!(dev.groups(), vec!["Main", "General Info"]);

        let changes = apply_xml(&mut cache, r#"
<setSwitchVector device="Cam" name="CONNECTION" state="Ok">
  <oneSwitch name="CONNECT">On</oneSwitch><oneSwitch name="DISCONNECT">Off</oneSwitch>
</setSwitchVector>
<setNumberVector device="Cam" name="CCD_TEMPERATURE" state="Busy">
  <oneNumber name="CCD_TEMPERATURE_VALUE">-10.5</oneNumber>
</setNumberVector>
<setNumberVector device="Cam" name="UNKNOWN"><oneNumber name="X">1</oneNumber></setNumberVector>"#);
        assert_eq!(changes[2], Change::None);
        let dev = &cache.devices["Cam"];
        assert!(dev.is_connected());
        let t = &dev.properties["CCD_TEMPERATURE"];
        assert_eq!(t.state, PropertyState::Busy);
        assert_eq!(t.number("CCD_TEMPERATURE_VALUE"), Some(-10.5));
        let ElementValue::Number(n) = &t.elements[0].value else {
            panic!()
        };
        assert_eq!((n.min, n.max, n.format.as_str()), (-50.0, 50.0, "%.2f"));

        apply_xml(&mut cache, r#"<delProperty device="Cam" name="CCD_TEMPERATURE"/>"#);
        assert!(cache.property("Cam", "CCD_TEMPERATURE").is_none());
        assert!(cache.property("Cam", "CONNECTION").is_some());
        apply_xml(&mut cache, r#"<delProperty device="Cam"/>"#);
        assert!(cache.devices.is_empty());
    }
}
