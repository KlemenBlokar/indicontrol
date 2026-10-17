//! Helpers for reading standard INDI properties, shared by engine and UIs.

use indi_link::{Device, ElementValue, NumberValue};

/// Camera controls whose INDI property differs between drivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Gain,
    Offset,
}

/// `(property, element)` holding a camera control: `CCD_GAIN.GAIN` /
/// `CCD_OFFSET.OFFSET` (e.g. QHY, simulator) or an element of
/// `CCD_CONTROLS` (e.g. ZWO, ToupTek).
pub fn control_location(camera: &Device, control: Control) -> Option<(String, String)> {
    let (prop, el, alias) = match control {
        Control::Gain => ("CCD_GAIN", "GAIN", "gain"),
        Control::Offset => ("CCD_OFFSET", "OFFSET", "offset"),
    };
    if camera
        .properties
        .get(prop)
        .is_some_and(|p| p.element(el).is_some())
    {
        return Some((prop.into(), el.into()));
    }
    let controls = camera.properties.get("CCD_CONTROLS")?;
    controls
        .elements
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(alias))
        .map(|e| ("CCD_CONTROLS".into(), e.name.clone()))
}

pub fn control_value(camera: &Device, control: Control) -> Option<&NumberValue> {
    let (prop, el) = control_location(camera, control)?;
    match &camera.properties.get(&prop)?.element(&el)?.value {
        ElementValue::Number(n) => Some(n),
        _ => None,
    }
}

pub fn number<'a>(dev: &'a Device, prop: &str, el: &str) -> Option<&'a NumberValue> {
    match &dev.properties.get(prop)?.element(el)?.value {
        ElementValue::Number(n) => Some(n),
        _ => None,
    }
}

/// Filter names by slot (index 0 = slot 1).
pub fn filter_names(wheel: &Device) -> Vec<String> {
    let Some(p) = wheel.properties.get("FILTER_NAME") else {
        // Unnamed wheel: number the slots.
        let max = number(wheel, "FILTER_SLOT", "FILTER_SLOT_VALUE").map_or(0.0, |n| n.max);
        return (1..=max.max(0.0) as u32).map(|i| format!("Filter {i}")).collect();
    };
    p.elements
        .iter()
        .enumerate()
        .map(|(i, e)| match &e.value {
            ElementValue::Text(t) if !t.trim().is_empty() => t.trim().to_owned(),
            _ => format!("Filter {}", i + 1),
        })
        .collect()
}

/// Name of a 1-based filter slot.
pub fn filter_name(wheel: &Device, slot: u32) -> Option<String> {
    let index = (slot as usize).checked_sub(1)?;
    filter_names(wheel).get(index).cloned()
}

pub fn current_filter_slot(wheel: &Device) -> Option<u32> {
    number(wheel, "FILTER_SLOT", "FILTER_SLOT_VALUE").map(|n| n.value.round() as u32)
}
