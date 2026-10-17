//! Minimal FITS (primary HDU) reader/writer.
//!
//! Enough for camera frames: we parse the header, keep the data unit as raw
//! big-endian bytes, and can write a modified header in front of the
//! untouched data — so saving never re-encodes pixels.

use std::fmt::Write as _;

pub const BLOCK: usize = 2880;
const CARD: usize = 80;

#[derive(Debug, thiserror::Error)]
pub enum FitsError {
    #[error("not a FITS file")]
    NotFits,
    #[error("FITS header has no END card")]
    NoEnd,
    #[error("missing or invalid {0}")]
    BadKeyword(&'static str),
    #[error("data unit truncated: need {need} bytes, have {have}")]
    Truncated { need: usize, have: usize },
    #[error("unsupported: {0}")]
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Logical(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Commentary cards (COMMENT, HISTORY, blank) and cards without a value.
    None,
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Value as it would appear to a user (no quotes).
    pub fn display(&self) -> String {
        match self {
            Value::Logical(b) => if *b { "T" } else { "F" }.into(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => s.clone(),
            Value::None => String::new(),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_owned())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Self {
        Value::Float(f)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Logical(b)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub key: String,
    pub value: Value,
    pub comment: Option<String>,
}

impl Card {
    pub fn new(key: &str, value: impl Into<Value>, comment: &str) -> Self {
        Card {
            key: key.to_ascii_uppercase(),
            value: value.into(),
            comment: (!comment.is_empty()).then(|| comment.to_owned()),
        }
    }

    fn parse(raw: &[u8]) -> Card {
        let text: String = raw.iter().map(|&b| if b.is_ascii() { b as char } else { '?' }).collect();
        let key = text.get(..8).unwrap_or(&text).trim_end().to_owned();
        let rest = text.get(8..).unwrap_or("");
        let Some(field) = rest.strip_prefix("= ") else {
            let comment = rest.trim_end();
            return Card {
                key,
                value: Value::None,
                comment: (!comment.is_empty()).then(|| comment.to_owned()),
            };
        };
        let field = field.trim_start();
        let (value, comment) = if let Some(s) = field.strip_prefix('\'') {
            // Quoted string; '' is an escaped quote.
            let mut out = String::new();
            let mut chars = s.char_indices().peekable();
            let mut end = s.len();
            while let Some((i, c)) = chars.next() {
                if c == '\'' {
                    if chars.peek().map(|&(_, n)| n) == Some('\'') {
                        out.push('\'');
                        chars.next();
                    } else {
                        end = i + 1;
                        break;
                    }
                } else {
                    out.push(c);
                }
            }
            let after = &s[end.min(s.len())..];
            (Value::Str(out.trim_end().to_owned()), split_comment(after))
        } else {
            let (v, c) = field.split_once('/').unwrap_or((field, ""));
            let v = v.trim();
            let value = match v {
                "T" => Value::Logical(true),
                "F" => Value::Logical(false),
                "" => Value::None,
                _ => v
                    .parse::<i64>()
                    .map(Value::Int)
                    .or_else(|_| v.replace(['D', 'd'], "E").parse::<f64>().map(Value::Float))
                    .unwrap_or_else(|_| Value::Str(v.to_owned())),
            };
            let c = c.trim();
            (value, (!c.is_empty()).then(|| c.to_owned()))
        };
        Card { key, value, comment }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let mut s = format!("{:<8}", self.key);
        s.truncate(8);
        match &self.value {
            Value::None => {
                s.push_str(self.comment.as_deref().unwrap_or(""));
            }
            v => {
                s.push_str("= ");
                match v {
                    Value::Str(text) => {
                        let escaped = text.replace('\'', "''");
                        let _ = write!(s, "'{escaped:<8}'");
                    }
                    other => {
                        let _ = write!(s, "{:>20}", other.display());
                    }
                }
                if let Some(c) = &self.comment
                    && s.len() + 3 < CARD
                {
                    let _ = write!(s, " / {c}");
                }
            }
        }
        let mut bytes: Vec<u8> = s
            .chars()
            .map(|c| if c.is_ascii() && !c.is_ascii_control() { c as u8 } else { b'?' })
            .collect();
        bytes.resize(CARD, b' ');
        out.extend_from_slice(&bytes);
    }
}

fn split_comment(s: &str) -> Option<String> {
    let c = s.trim_start().strip_prefix('/')?.trim();
    (!c.is_empty()).then(|| c.to_owned())
}

/// FITS-friendly float formatting (always contains '.' or 'E').
fn format_float(f: f64) -> String {
    if !f.is_finite() {
        return "0.0".into();
    }
    let s = format!("{f}");
    let positional = f == 0.0 || (1e-4..1e15).contains(&f.abs());
    if positional && s.len() <= 20 {
        if s.contains('.') { s } else { format!("{s}.0") }
    } else {
        let e = format!("{f:.12E}");
        let (m, exp) = e.split_once('E').unwrap_or((&e, "0"));
        let exp: i32 = exp.parse().unwrap_or(0);
        let m = m.trim_end_matches('0');
        let m = if m.ends_with('.') { format!("{m}0") } else { m.to_owned() };
        format!("{m}E{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Header {
    pub cards: Vec<Card>,
}

impl Header {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.cards.iter().find(|c| c.key == key).map(|c| &c.value)
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Int(i) => Some(*i),
            Value::Float(f) if f.fract() == 0.0 => Some(*f as i64),
            _ => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Replace an existing card's value (keeping its position) or append.
    pub fn set(&mut self, card: Card) {
        match self.cards.iter_mut().find(|c| c.key == card.key) {
            Some(existing) => {
                existing.value = card.value;
                if card.comment.is_some() {
                    existing.comment = card.comment;
                }
            }
            None => self.cards.push(card),
        }
    }

    /// Add the card only if the key is not present yet.
    pub fn set_default(&mut self, card: Card) {
        if !self.contains(&card.key) {
            self.cards.push(card);
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.cards.retain(|c| c.key != key);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity((self.cards.len() + 1) * CARD);
        for c in &self.cards {
            c.write(&mut out);
        }
        let mut end = b"END".to_vec();
        end.resize(CARD, b' ');
        out.extend_from_slice(&end);
        pad(&mut out, b' ');
        out
    }
}

fn pad(buf: &mut Vec<u8>, fill: u8) {
    let rem = buf.len() % BLOCK;
    if rem != 0 {
        buf.resize(buf.len() + BLOCK - rem, fill);
    }
}

/// Primary HDU of a FITS file.
#[derive(Debug, Clone)]
pub struct Fits {
    pub header: Header,
    /// Raw big-endian data unit, without padding.
    pub data: Vec<u8>,
}

impl Fits {
    pub fn parse(bytes: &[u8]) -> Result<Fits, FitsError> {
        if !bytes.starts_with(b"SIMPLE  =") {
            return Err(FitsError::NotFits);
        }
        let mut header = Header::default();
        let mut end = None;
        for (i, raw) in bytes.as_chunks::<CARD>().0.iter().enumerate() {
            if raw.starts_with(b"END") && raw[3..].iter().all(|&b| b == b' ') {
                end = Some((i + 1) * CARD);
                break;
            }
            header.cards.push(Card::parse(raw));
        }
        let header_len = end.ok_or(FitsError::NoEnd)?.div_ceil(BLOCK) * BLOCK;
        let len = data_len(&header)?;
        let have = bytes.len().saturating_sub(header_len);
        if have < len {
            return Err(FitsError::Truncated { need: len, have });
        }
        Ok(Fits {
            data: bytes[header_len..header_len + len].to_vec(),
            header,
        })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.header.to_bytes();
        out.extend_from_slice(&self.data);
        pad(&mut out, 0);
        out
    }

    pub fn bitpix(&self) -> Result<i64, FitsError> {
        self.header.get_i64("BITPIX").ok_or(FitsError::BadKeyword("BITPIX"))
    }

    /// (NAXIS1, NAXIS2, NAXIS3 or 1)
    pub fn dims(&self) -> Result<(usize, usize, usize), FitsError> {
        let naxis = self.header.get_i64("NAXIS").ok_or(FitsError::BadKeyword("NAXIS"))?;
        if !(2..=3).contains(&naxis) {
            return Err(FitsError::Unsupported(format!("NAXIS = {naxis}")));
        }
        let ax = |k: &'static str| {
            self.header
                .get_i64(k)
                .filter(|&v| v > 0)
                .map(|v| v as usize)
                .ok_or(FitsError::BadKeyword(k))
        };
        let depth = if naxis == 3 { ax("NAXIS3")? } else { 1 };
        Ok((ax("NAXIS1")?, ax("NAXIS2")?, depth))
    }
}

fn data_len(h: &Header) -> Result<usize, FitsError> {
    let bitpix = h.get_i64("BITPIX").ok_or(FitsError::BadKeyword("BITPIX"))?;
    let naxis = h.get_i64("NAXIS").ok_or(FitsError::BadKeyword("NAXIS"))?;
    if naxis == 0 {
        return Ok(0);
    }
    let mut n = (bitpix.unsigned_abs() / 8) as usize;
    for i in 1..=naxis {
        let k = format!("NAXIS{i}");
        let v = h.get_i64(&k).ok_or(FitsError::BadKeyword("NAXISn"))?;
        n = n.saturating_mul(v.max(0) as usize);
    }
    Ok(n)
}

#[cfg(test)]
pub(crate) use tests::sample_fits;

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample_fits(width: usize, height: usize, pixels: &[u16]) -> Vec<u8> {
        let mut h = Header::default();
        h.set(Card::new("SIMPLE", true, "conforms to FITS standard"));
        h.set(Card::new("BITPIX", 16i64, "bits per pixel"));
        h.set(Card::new("NAXIS", 2i64, ""));
        h.set(Card::new("NAXIS1", width as i64, ""));
        h.set(Card::new("NAXIS2", height as i64, ""));
        h.set(Card::new("BZERO", 32768i64, ""));
        h.set(Card::new("BSCALE", 1i64, ""));
        h.set(Card::new("INSTRUME", "CCD Simulator", "camera"));
        let data: Vec<u8> = pixels
            .iter()
            .flat_map(|&p| ((p as i32 - 32768) as i16).to_be_bytes())
            .collect();
        Fits { header: h, data }.to_bytes()
    }

    #[test]
    fn header_round_trip() {
        let mut h = Header::default();
        h.set(Card::new("SIMPLE", true, ""));
        h.set(Card::new("EXPTIME", 120.0, "Exposure time [s]"));
        h.set(Card::new("OBJECT", "M 31 'Andromeda'", ""));
        h.set(Card::new("GAIN", 100i64, ""));
        h.set(Card::new("TINY", 1.5e-12, ""));
        h.cards.push(Card {
            key: "HISTORY".into(),
            value: Value::None,
            comment: Some("made by a test".into()),
        });
        let bytes = h.to_bytes();
        assert_eq!(bytes.len(), BLOCK);
        let text = std::str::from_utf8(&bytes[..CARD * 3]).unwrap();
        assert!(text.contains("EXPTIME =                120.0 / Exposure time [s]"));
        assert!(text.contains("OBJECT  = 'M 31 ''Andromeda'''"));

        let parsed: Vec<Card> = bytes
            .as_chunks::<CARD>()
            .0
            .iter()
            .take_while(|c| !c.starts_with(b"END"))
            .map(|c| Card::parse(c))
            .collect();
        assert_eq!(parsed, h.cards);
    }

    #[test]
    fn parses_driver_style_cards() {
        let card = |s: &str| {
            let mut b = s.as_bytes().to_vec();
            b.resize(CARD, b' ');
            Card::parse(&b)
        };
        assert_eq!(card("BZERO   =                32768 / offset").value, Value::Int(32768));
        assert_eq!(card("CCD-TEMP=  -1.00000000000000E+01").value, Value::Float(-10.0));
        assert_eq!(card("PIXSIZE1=    3.7599999904632568D0").value.as_f64(), Some(3.759999990463257));
        assert_eq!(card("FRAME   = 'Light   '           / Frame Type").value, Value::Str("Light".into()));
        assert_eq!(card("FRAME   = 'Light   '           / Frame Type").comment.as_deref(), Some("Frame Type"));
        assert_eq!(card("ROWORDER= 'TOP-DOWN'").value.as_str(), Some("TOP-DOWN"));
        assert_eq!(card("COMMENT hello world").comment.as_deref(), Some("hello world"));
    }

    #[test]
    fn file_round_trip_keeps_data() {
        let pixels: Vec<u16> = (0..12).map(|i| i * 5000).collect();
        let bytes = sample_fits(4, 3, &pixels);
        assert_eq!(bytes.len() % BLOCK, 0);
        let mut fits = Fits::parse(&bytes).unwrap();
        assert_eq!(fits.dims().unwrap(), (4, 3, 1));
        assert_eq!(fits.header.get_str("INSTRUME"), Some("CCD Simulator"));
        fits.header.set(Card::new("FILTER", "Ha", "filter name"));
        let again = Fits::parse(&fits.to_bytes()).unwrap();
        assert_eq!(again.header.get_str("FILTER"), Some("Ha"));
        assert_eq!(again.data, fits.data);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(matches!(Fits::parse(b"hello"), Err(FitsError::NotFits)));
        let mut bytes = sample_fits(4, 3, &[0; 12]);
        bytes.truncate(BLOCK + 10);
        assert!(matches!(Fits::parse(&bytes), Err(FitsError::Truncated { .. })));
    }

    #[test]
    fn floats_format_for_fits() {
        assert_eq!(format_float(120.0), "120.0");
        assert_eq!(format_float(-3.75), "-3.75");
        assert_eq!(format_float(1.5e-12), "1.5E-12");
        assert_eq!(format_float(6.02e23), "6.02E+23");
    }
}

