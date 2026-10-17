//! Turns a stream of quick-xml events into INDI protocol messages.
//!
//! An INDI connection is a sequence of top-level XML elements with no root.
//! Each message is at most two levels deep (a vector and its members), so we
//! collect each top-level element into a small [`Node`] tree and convert it.

use std::io::Read;
use std::sync::Arc;

use base64::Engine;
use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};

use crate::format::parse_number;
use crate::types::*;

#[derive(Debug, Clone, PartialEq)]
pub enum IndiMessage {
    Def {
        property: Property,
        message: Option<String>,
    },
    Set(SetVector),
    Del {
        device: String,
        name: Option<String>,
        timestamp: Option<String>,
        message: Option<String>,
    },
    Message {
        device: Option<String>,
        timestamp: Option<String>,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetVector {
    pub device: String,
    pub name: String,
    pub kind: PropertyKind,
    pub state: Option<PropertyState>,
    pub timeout: Option<f64>,
    pub timestamp: Option<String>,
    pub message: Option<String>,
    pub elements: Vec<SetElement>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetElement {
    pub name: String,
    pub value: SetValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetValue {
    Text(String),
    Number {
        value: f64,
        min: Option<f64>,
        max: Option<f64>,
        step: Option<f64>,
    },
    Switch(bool),
    Light(PropertyState),
    Blob(BlobValue),
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unknown message <{0}>")]
    UnknownTag(String),
    #[error("<{tag}> is missing attribute '{attr}'")]
    MissingAttr { tag: String, attr: &'static str },
    #[error("bad value '{value}' in {device}.{name}.{element}")]
    BadValue {
        device: String,
        name: String,
        element: String,
        value: String,
    },
    #[error("BLOB decode failed for {device}.{name}: {reason}")]
    Blob {
        device: String,
        name: String,
        reason: String,
    },
}

#[derive(Debug, Default)]
struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn from_start(e: &BytesStart) -> Self {
        let tag = e.name().as_ref().to_owned();
        let attrs = e
            .attributes()
            .with_checks(false)
            .flatten()
            .map(|a| {
                let key = a.key.as_ref().to_owned();
                let value = a
                    .normalized_value(XmlVersion::Implicit1_0)
                    .map(|v| v.into_owned())
                    .unwrap_or_else(|_| a.value.clone().into_owned());
                (key, value)
            })
            .collect();
        Node {
            tag,
            attrs,
            ..Default::default()
        }
    }

    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn require(&self, key: &'static str) -> Result<String, ParseError> {
        self.attr(key)
            .map(str::to_owned)
            .ok_or_else(|| ParseError::MissingAttr {
                tag: self.tag.clone(),
                attr: key,
            })
    }

    fn opt(&self, key: &str) -> Option<String> {
        self.attr(key).map(str::to_owned)
    }
}

/// Incremental parser fed with quick-xml events.
#[derive(Default)]
pub struct Parser {
    depth: usize,
    top: Option<Node>,
    child: Option<Node>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one event. Returns a message when a top-level element completes.
    pub fn feed(&mut self, event: Event) -> Option<Result<IndiMessage, ParseError>> {
        match event {
            Event::Start(e) => {
                self.depth += 1;
                match self.depth {
                    1 => self.top = Some(Node::from_start(&e)),
                    2 => self.child = Some(Node::from_start(&e)),
                    _ => {}
                }
            }
            Event::Empty(e) => match self.depth {
                0 => return Some(convert(Node::from_start(&e))),
                1 => {
                    if let Some(top) = &mut self.top {
                        top.children.push(Node::from_start(&e));
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if let Some(child) = self.child.as_mut().filter(|_| self.depth == 2) {
                    child.text.push_str(&t.xml10_content());
                } else if let Some(top) = self.top.as_mut().filter(|_| self.depth == 1) {
                    // Only <message>-like elements carry top-level text; skip whitespace.
                    let s = t.xml10_content();
                    if !s.trim().is_empty() {
                        top.text.push_str(&s);
                    }
                }
            }
            Event::CData(c) => {
                if let Some(child) = self.child.as_mut().filter(|_| self.depth == 2) {
                    child.text.push_str(&c.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref() {
                    Ok(Some(c)) => Some(c.to_string()),
                    _ => resolve_predefined_entity(&r).map(str::to_owned),
                };
                let target = match self.depth {
                    2 => self.child.as_mut(),
                    1 => self.top.as_mut(),
                    _ => None,
                };
                if let (Some(node), Some(s)) = (target, resolved) {
                    node.text.push_str(&s);
                }
            }
            Event::End(_) => {
                match self.depth {
                    2 => {
                        if let (Some(top), Some(child)) = (&mut self.top, self.child.take()) {
                            top.children.push(child);
                        }
                    }
                    1 => {
                        self.depth = 0;
                        return self.top.take().map(convert);
                    }
                    _ => {}
                }
                self.depth = self.depth.saturating_sub(1);
            }
            _ => {}
        }
        None
    }
}

fn convert(node: Node) -> Result<IndiMessage, ParseError> {
    let tag = node.tag.as_str();
    if tag == "message" {
        return Ok(IndiMessage::Message {
            device: node.opt("device"),
            timestamp: node.opt("timestamp"),
            message: node.opt("message").unwrap_or_default(),
        });
    }
    if tag == "delProperty" {
        return Ok(IndiMessage::Del {
            device: node.require("device")?,
            name: node.opt("name"),
            timestamp: node.opt("timestamp"),
            message: node.opt("message"),
        });
    }
    let kind_of = |prefix: &str| {
        tag.strip_prefix(prefix)
            .and_then(|r| r.strip_suffix("Vector"))
            .and_then(PropertyKind::from_tag)
    };
    if let Some(kind) = kind_of("def") {
        return convert_def(node, kind);
    }
    if let Some(kind) = kind_of("set") {
        return convert_set(node, kind);
    }
    Err(ParseError::UnknownTag(node.tag))
}

fn bad_value(device: &str, name: &str, element: &str, value: &str) -> ParseError {
    ParseError::BadValue {
        device: device.into(),
        name: name.into(),
        element: element.into(),
        value: value.into(),
    }
}

fn parse_switch(s: &str) -> Option<bool> {
    match s.trim() {
        "On" => Some(true),
        "Off" => Some(false),
        _ => None,
    }
}

fn convert_def(node: Node, kind: PropertyKind) -> Result<IndiMessage, ParseError> {
    let device = node.require("device")?;
    let name = node.require("name")?;
    let mut elements = Vec::with_capacity(node.children.len());
    for child in &node.children {
        let Some(el_name) = child.attr("name") else {
            continue;
        };
        let text = child.text.trim();
        let value = match kind {
            PropertyKind::Text => ElementValue::Text(text.to_owned()),
            PropertyKind::Number => {
                let num = |key: &str| child.attr(key).and_then(parse_number).unwrap_or(0.0);
                ElementValue::Number(NumberValue {
                    value: parse_number(text)
                        .ok_or_else(|| bad_value(&device, &name, el_name, text))?,
                    min: num("min"),
                    max: num("max"),
                    step: num("step"),
                    format: child.opt("format").unwrap_or_else(|| "%g".into()),
                })
            }
            PropertyKind::Switch => ElementValue::Switch(
                parse_switch(text).ok_or_else(|| bad_value(&device, &name, el_name, text))?,
            ),
            PropertyKind::Light => ElementValue::Light(
                PropertyState::parse(text)
                    .ok_or_else(|| bad_value(&device, &name, el_name, text))?,
            ),
            PropertyKind::Blob => ElementValue::Blob(BlobValue {
                format: String::new(),
                data: Arc::new(Vec::new()),
            }),
        };
        elements.push(Element {
            name: el_name.to_owned(),
            label: child
                .opt("label")
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| el_name.to_owned()),
            value,
        });
    }
    let property = Property {
        label: node
            .opt("label")
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| name.clone()),
        group: node.opt("group").unwrap_or_default(),
        kind,
        state: node
            .attr("state")
            .and_then(PropertyState::parse)
            .unwrap_or_default(),
        perm: match kind {
            PropertyKind::Light => Permission::ReadOnly,
            _ => node
                .attr("perm")
                .and_then(Permission::parse)
                .unwrap_or_default(),
        },
        rule: node
            .attr("rule")
            .and_then(SwitchRule::parse)
            .unwrap_or_default(),
        timeout: node.attr("timeout").and_then(parse_number).unwrap_or(0.0),
        timestamp: node.opt("timestamp"),
        elements,
        device,
        name,
    };
    Ok(IndiMessage::Def {
        property,
        message: node.opt("message"),
    })
}

fn convert_set(node: Node, kind: PropertyKind) -> Result<IndiMessage, ParseError> {
    let device = node.require("device")?;
    let name = node.require("name")?;
    let mut elements = Vec::with_capacity(node.children.len());
    for child in &node.children {
        let Some(el_name) = child.attr("name") else {
            continue;
        };
        let text = child.text.trim();
        let value = match kind {
            PropertyKind::Text => SetValue::Text(text.to_owned()),
            PropertyKind::Number => SetValue::Number {
                value: parse_number(text)
                    .ok_or_else(|| bad_value(&device, &name, el_name, text))?,
                min: child.attr("min").and_then(parse_number),
                max: child.attr("max").and_then(parse_number),
                step: child.attr("step").and_then(parse_number),
            },
            PropertyKind::Switch => SetValue::Switch(
                parse_switch(text).ok_or_else(|| bad_value(&device, &name, el_name, text))?,
            ),
            PropertyKind::Light => SetValue::Light(
                PropertyState::parse(text)
                    .ok_or_else(|| bad_value(&device, &name, el_name, text))?,
            ),
            PropertyKind::Blob => {
                let format = child.opt("format").unwrap_or_default();
                let data = decode_blob(text, &format).map_err(|reason| ParseError::Blob {
                    device: device.clone(),
                    name: name.clone(),
                    reason,
                })?;
                SetValue::Blob(BlobValue {
                    format: format.strip_suffix(".z").unwrap_or(&format).to_owned(),
                    data: Arc::new(data),
                })
            }
        };
        elements.push(SetElement {
            name: el_name.to_owned(),
            value,
        });
    }
    Ok(IndiMessage::Set(SetVector {
        state: node.attr("state").and_then(PropertyState::parse),
        timeout: node.attr("timeout").and_then(parse_number),
        timestamp: node.opt("timestamp"),
        message: node.opt("message"),
        device,
        name,
        kind,
        elements,
    }))
}

/// Base64-decode a BLOB payload, inflating it if the format ends in `.z`.
fn decode_blob(text: &str, format: &str) -> Result<Vec<u8>, String> {
    let compact: Vec<u8> = text
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(&compact)
        .map_err(|e| e.to_string())?;
    if format.ends_with(".z") {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(raw.as_slice())
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())?;
        Ok(out)
    } else {
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::Reader;

    pub(crate) fn parse_all(xml: &str) -> Vec<Result<IndiMessage, ParseError>> {
        let mut reader = Reader::from_str(xml);
        let mut parser = Parser::new();
        let mut out = Vec::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Eof => break,
                ev => out.extend(parser.feed(ev)),
            }
        }
        out
    }

    fn one(xml: &str) -> IndiMessage {
        let mut msgs = parse_all(xml);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        msgs.pop().unwrap().unwrap()
    }

    #[test]
    fn def_number_vector() {
        let msg = one(r#"
<defNumberVector device="Telescope Simulator" name="EQUATORIAL_EOD_COORD" label="Eq. Coordinates" group="Main Control" state="Idle" perm="rw" timeout="60" timestamp="2026-10-17T20:00:00">
    <defNumber name="RA" label="RA (hh:mm:ss)" format="%010.6m" min="0" max="24" step="0">
      5.5
    </defNumber>
    <defNumber name="DEC" label="DEC (dd:mm:ss)" format="%010.6m" min="-90" max="90" step="0">
      -30:15:00
    </defNumber>
</defNumberVector>"#);
        let IndiMessage::Def { property: p, .. } = msg else {
            panic!()
        };
        assert_eq!(p.kind, PropertyKind::Number);
        assert_eq!(p.device, "Telescope Simulator");
        assert_eq!(p.group, "Main Control");
        assert_eq!(p.timeout, 60.0);
        assert_eq!(p.number("RA"), Some(5.5));
        assert_eq!(p.number("DEC"), Some(-30.25));
        let ElementValue::Number(dec) = &p.element("DEC").unwrap().value else {
            panic!()
        };
        assert_eq!((dec.min, dec.max), (-90.0, 90.0));
    }

    #[test]
    fn def_switch_text_light_and_entities() {
        let msgs = parse_all(r#"
<defSwitchVector device="CCD Simulator" name="CONNECTION" label="Connection" group="Main Control" state="Idle" perm="rw" rule="OneOfMany" timeout="60">
  <defSwitch name="CONNECT" label="Connect">Off</defSwitch>
  <defSwitch name="DISCONNECT" label="Disconnect">On</defSwitch>
</defSwitchVector>
<defTextVector device="CCD Simulator" name="DRIVER_INFO" group="General Info" state="Idle" perm="ro">
  <defText name="DRIVER_NAME">CCD &amp; &lt;Sim&gt; &#x41;</defText>
  <defText name="EMPTY"></defText>
</defTextVector>
<defLightVector device="CCD Simulator" name="STATUS" group="Main" state="Ok">
  <defLight name="L1">Alert</defLight>
</defLightVector>
<defBLOBVector device="CCD Simulator" name="CCD1" group="Image" state="Idle" perm="ro">
  <defBLOB name="CCD1"/>
</defBLOBVector>"#);
        let props: Vec<Property> = msgs
            .into_iter()
            .map(|m| match m.unwrap() {
                IndiMessage::Def { property, .. } => property,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(props.len(), 4);
        assert_eq!(props[0].rule, SwitchRule::OneOfMany);
        assert_eq!(props[0].active_switch(), Some("DISCONNECT"));
        assert_eq!(props[1].perm, Permission::ReadOnly);
        assert_eq!(props[1].label, "DRIVER_INFO");
        assert_eq!(props[1].text("DRIVER_NAME"), Some("CCD & <Sim> A"));
        assert_eq!(props[1].text("EMPTY"), Some(""));
        assert_eq!(props[2].perm, Permission::ReadOnly);
        assert_eq!(
            props[2].element("L1").unwrap().value,
            ElementValue::Light(PropertyState::Alert)
        );
        assert_eq!(props[3].kind, PropertyKind::Blob);
        assert_eq!(props[3].elements.len(), 1);
    }

    #[test]
    fn set_number_and_message_and_del() {
        let msgs = parse_all(r#"
<setNumberVector device="CCD Simulator" name="CCD_EXPOSURE" state="Busy" timeout="60" message="exposing">
  <oneNumber name="CCD_EXPOSURE_VALUE" min="0" max="3600">4.5</oneNumber>
</setNumberVector>
<message device="CCD Simulator" timestamp="2026-10-17T20:00:01" message="Exposure done."/>
<delProperty device="CCD Simulator" name="CCD_TEMPERATURE"/>
<delProperty device="Gone"/>"#);
        let msgs: Vec<_> = msgs.into_iter().map(Result::unwrap).collect();
        let IndiMessage::Set(set) = &msgs[0] else {
            panic!()
        };
        assert_eq!(set.state, Some(PropertyState::Busy));
        assert_eq!(set.message.as_deref(), Some("exposing"));
        assert_eq!(
            set.elements[0].value,
            SetValue::Number {
                value: 4.5,
                min: Some(0.0),
                max: Some(3600.0),
                step: None
            }
        );
        assert!(matches!(&msgs[1], IndiMessage::Message { message, .. } if message == "Exposure done."));
        assert!(matches!(&msgs[2], IndiMessage::Del { name: Some(n), .. } if n == "CCD_TEMPERATURE"));
        assert!(matches!(&msgs[3], IndiMessage::Del { name: None, .. }));
    }

    #[test]
    fn set_blob_plain_and_compressed() {
        use std::io::Write;
        let payload = b"SIMPLE  =                    T".repeat(10);
        let plain = base64::engine::general_purpose::STANDARD.encode(&payload);
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&payload).unwrap();
        let zipped = base64::engine::general_purpose::STANDARD.encode(enc.finish().unwrap());
        // Drivers wrap base64 at 72 columns.
        let wrapped: String = plain
            .as_bytes()
            .chunks(72)
            .map(|c| format!("{}\n", std::str::from_utf8(c).unwrap()))
            .collect();
        let xml = format!(
            r#"<setBLOBVector device="CCD" name="CCD1" state="Ok">
  <oneBLOB name="CCD1" size="{n}" format=".fits">
{wrapped}  </oneBLOB>
</setBLOBVector>
<setBLOBVector device="CCD" name="CCD1" state="Ok"><oneBLOB name="CCD1" size="{n}" format=".fits.z">{zipped}</oneBLOB></setBLOBVector>"#,
            n = payload.len()
        );
        for msg in parse_all(&xml) {
            let IndiMessage::Set(set) = msg.unwrap() else {
                panic!()
            };
            let SetValue::Blob(b) = &set.elements[0].value else {
                panic!()
            };
            assert_eq!(b.format, ".fits");
            assert_eq!(b.data.as_slice(), payload.as_slice());
        }
    }

    #[test]
    fn bad_values_are_errors_but_stream_continues() {
        let msgs = parse_all(r#"
<setSwitchVector device="D" name="S"><oneSwitch name="A">Maybe</oneSwitch></setSwitchVector>
<getProperties version="1.7"/>
<setSwitchVector device="D" name="S"><oneSwitch name="A">On</oneSwitch></setSwitchVector>"#);
        assert_eq!(msgs.len(), 3);
        assert!(matches!(msgs[0], Err(ParseError::BadValue { .. })));
        assert!(matches!(msgs[1], Err(ParseError::UnknownTag(_))));
        assert!(msgs[2].is_ok());
    }
}
