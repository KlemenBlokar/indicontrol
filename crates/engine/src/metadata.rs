//! Frame metadata gathered from the equipment when an exposure starts.

use chrono::{DateTime, Utc};
use imaging::astro;
use imaging::fits::Card;
use indi_link::{Cache, Device};
use protocol::equipment::{self, Control};
use protocol::{CaptureRequest, FrameType, Roles};

#[derive(Debug, Clone)]
pub struct FrameContext {
    pub camera: String,
    pub frame_type: FrameType,
    pub exposure: f64,
    pub object: String,
    pub filter: Option<String>,
    pub start: DateTime<Utc>,
    pub gain: Option<f64>,
    pub offset: Option<f64>,
    pub temperature: Option<f64>,
    pub binning: Option<(u32, u32)>,
    pub bits_per_pixel: Option<u32>,
    /// Always written, replacing whatever the driver put there.
    pub cards: Vec<Card>,
    /// Written only if the driver did not provide them.
    pub defaults: Vec<Card>,
}

pub fn collect(
    cache: &Cache,
    roles: &Roles,
    req: &CaptureRequest,
    camera: &str,
    start: DateTime<Utc>,
) -> FrameContext {
    let dev = |name: &Option<String>| name.as_deref().and_then(|n| cache.devices.get(n));
    let cam = cache.devices.get(camera);
    let mount = dev(&roles.mount);
    let wheel = dev(&roles.filter_wheel);
    let focuser = dev(&roles.focuser);
    let num = |d: Option<&Device>, p: &str, e: &str| d.and_then(|d| equipment::number(d, p, e)).map(|n| n.value);

    let gain = req
        .gain
        .or_else(|| cam.and_then(|c| equipment::control_value(c, Control::Gain)).map(|n| n.value));
    let offset = req
        .offset
        .or_else(|| cam.and_then(|c| equipment::control_value(c, Control::Offset)).map(|n| n.value));
    let temperature = num(cam, "CCD_TEMPERATURE", "CCD_TEMPERATURE_VALUE");
    let binning = match req.binning {
        Some(b) => Some((b, b)),
        None => num(cam, "CCD_BINNING", "HOR_BIN")
            .zip(num(cam, "CCD_BINNING", "VER_BIN"))
            .map(|(h, v)| (h as u32, v as u32)),
    };
    let filter = wheel.and_then(|w| {
        let slot = req.filter_slot.or_else(|| equipment::current_filter_slot(w))?;
        equipment::filter_name(w, slot)
    });
    let bits_per_pixel = num(cam, "CCD_INFO", "CCD_BITSPERPIXEL").map(|b| b as u32);

    let mut cards = vec![
        Card::new("IMAGETYP", req.frame_type.imagetyp(), "Type of exposure"),
        Card::new("EXPTIME", req.exposure, "[s] Exposure duration"),
        Card::new("SWCREATE", format!("indicontrol {}", env!("CARGO_PKG_VERSION")), "Capture software"),
    ];
    if !req.object.trim().is_empty() {
        cards.push(Card::new("OBJECT", req.object.trim(), "Name of the object of interest"));
    }
    if let Some(f) = &filter {
        cards.push(Card::new("FILTER", f.as_str(), "Filter used"));
    }
    if let Some(g) = gain {
        cards.push(Card::new("GAIN", g, "Sensor gain"));
    }
    if let Some(o) = offset {
        cards.push(Card::new("OFFSET", o, "Sensor offset (bias pedestal)"));
    }
    if let Some(m) = &roles.mount {
        cards.push(Card::new("TELESCOP", m.as_str(), "Mount"));
    }
    // Optics: camera SCOPE_INFO (INDI 2.x), else mount TELESCOPE_INFO.
    let focal = num(cam, "SCOPE_INFO", "FOCAL_LENGTH")
        .filter(|f| *f > 0.0)
        .or_else(|| num(mount, "TELESCOPE_INFO", "TELESCOPE_FOCAL_LENGTH"))
        .filter(|f| *f > 0.0);
    let aperture = num(cam, "SCOPE_INFO", "APERTURE")
        .filter(|a| *a > 0.0)
        .or_else(|| num(mount, "TELESCOPE_INFO", "TELESCOPE_APERTURE"))
        .filter(|a| *a > 0.0);
    if let Some(f) = focal {
        cards.push(Card::new("FOCALLEN", f, "[mm] Focal length"));
    }
    if let Some(a) = aperture {
        cards.push(Card::new("APTDIA", a, "[mm] Aperture diameter"));
    }

    let lat = num(mount, "GEOGRAPHIC_COORD", "LAT");
    let lon = num(mount, "GEOGRAPHIC_COORD", "LONG").map(|l| if l > 180.0 { l - 360.0 } else { l });
    if let (Some(lat), Some(lon)) = (lat, lon) {
        cards.push(Card::new("SITELAT", lat, "[deg] Observatory latitude"));
        cards.push(Card::new("SITELONG", lon, "[deg] Observatory longitude, east positive"));
        if let Some(elev) = num(mount, "GEOGRAPHIC_COORD", "ELEV") {
            cards.push(Card::new("SITEELEV", elev, "[m] Observatory elevation"));
        }
    }

    // Pointing only makes sense for light frames.
    if req.frame_type == FrameType::Light
        && let Some(m) = mount
    {
        let jd = astro::julian_day(start.timestamp_millis() as f64 / 1000.0);
        let j2000 = num(Some(m), "EQUATORIAL_COORD", "RA").zip(num(Some(m), "EQUATORIAL_COORD", "DEC"));
        let jnow = num(Some(m), "EQUATORIAL_EOD_COORD", "RA").zip(num(Some(m), "EQUATORIAL_EOD_COORD", "DEC"));
        let radec = j2000.or_else(|| jnow.map(|(ra, dec)| astro::precess(ra, dec, jd, astro::J2000)));
        if let Some((ra, dec)) = radec {
            cards.push(Card::new("RA", ra * 15.0, "[deg] Pointing RA (J2000)"));
            cards.push(Card::new("DEC", dec, "[deg] Pointing Dec (J2000)"));
            cards.push(Card::new("OBJCTRA", astro::format_ra(ra), "[HMS] Pointing RA (J2000)"));
            cards.push(Card::new("OBJCTDEC", astro::format_dec(dec), "[DMS] Pointing Dec (J2000)"));
            cards.push(Card::new("EQUINOX", 2000.0, "Equinox of coordinates"));
        }
        if let (Some((ra, dec)), Some(lat), Some(lon)) = (jnow, lat, lon) {
            let alt = astro::altitude_deg(ra, dec, lat, lon, jd);
            cards.push(Card::new("CENTALT", alt, "[deg] Altitude at exposure start"));
            if let Some(am) = astro::airmass(alt) {
                cards.push(Card::new("AIRMASS", am, "Airmass at exposure start"));
            }
        }
        if let Some(side) = m
            .properties
            .get("TELESCOPE_PIER_SIDE")
            .and_then(|p| p.active_switch())
        {
            let side = match side {
                "PIER_WEST" => Some("WEST"),
                "PIER_EAST" => Some("EAST"),
                _ => None,
            };
            if let Some(s) = side {
                cards.push(Card::new("PIERSIDE", s, "Side of pier"));
            }
        }
    }

    if let Some(pos) = num(focuser, "ABS_FOCUS_POSITION", "FOCUS_ABSOLUTE_POSITION") {
        cards.push(Card::new("FOCPOS", pos, "Focuser position"));
        cards.push(Card::new("FOCUSPOS", pos, "Focuser position"));
    }
    if let Some(t) = num(focuser, "FOCUS_TEMPERATURE", "TEMPERATURE") {
        cards.push(Card::new("FOCTEMP", t, "[C] Focuser temperature"));
    }

    let mut defaults = vec![
        Card::new(
            "DATE-OBS",
            start.format("%Y-%m-%dT%H:%M:%S%.3f").to_string(),
            "UTC start of exposure",
        ),
        Card::new("INSTRUME", camera, "Camera"),
    ];
    if let Some(t) = temperature {
        defaults.push(Card::new("CCD-TEMP", t, "[C] Sensor temperature"));
    }
    if let Some((h, v)) = binning {
        defaults.push(Card::new("XBINNING", h as i64, "Binning factor, x"));
        defaults.push(Card::new("YBINNING", v as i64, "Binning factor, y"));
    }

    FrameContext {
        camera: camera.to_owned(),
        frame_type: req.frame_type,
        exposure: req.exposure,
        object: req.object.trim().to_owned(),
        filter,
        start,
        gain,
        offset,
        temperature,
        binning,
        bits_per_pixel,
        cards,
        defaults,
    }
}
