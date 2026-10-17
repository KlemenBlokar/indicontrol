//! Mount control (goto, park, tracking) and position reporting.

use imaging::astro;
use protocol::equipment;
use protocol::{LogLevel, MountInfo, ToUi};

use super::Core;

impl Core {
    pub(super) fn mount_info(&self) -> Option<MountInfo> {
        let name = self.state.roles.mount.clone()?;
        let cache = self.client.as_ref()?.cache();
        let m = cache.devices.get(&name)?;
        let eod = m.properties.get("EQUATORIAL_EOD_COORD")?;
        let (ra, dec) = (eod.number("RA")?, eod.number("DEC")?);
        let now = chrono::Utc::now();
        let jd = astro::julian_day(now.timestamp_millis() as f64 / 1000.0);
        let (ra_j2000, dec_j2000) = match (
            equipment::number(m, "EQUATORIAL_COORD", "RA"),
            equipment::number(m, "EQUATORIAL_COORD", "DEC"),
        ) {
            (Some(r), Some(d)) => (r.value, d.value),
            _ => astro::precess(ra, dec, jd, astro::J2000),
        };
        let site = equipment::number(m, "GEOGRAPHIC_COORD", "LAT")
            .zip(equipment::number(m, "GEOGRAPHIC_COORD", "LONG"))
            .map(|(lat, lon)| (lat.value, if lon.value > 180.0 { lon.value - 360.0 } else { lon.value }));
        let lst = site.map(|(_, lon)| ((astro::gmst_deg(jd) + lon) / 15.0).rem_euclid(24.0));
        let hour_angle = lst.map(|lst| (lst - ra + 12.0).rem_euclid(24.0) - 12.0);
        let alt = site.map(|(lat, lon)| astro::altitude_deg(ra, dec, lat, lon, jd));
        let pier_side = m
            .properties
            .get("TELESCOPE_PIER_SIDE")
            .and_then(|p| p.active_switch())
            .and_then(|s| match s {
                "PIER_WEST" => Some("WEST".to_owned()),
                "PIER_EAST" => Some("EAST".to_owned()),
                _ => None,
            });
        Some(MountInfo {
            name,
            ra,
            dec,
            ra_j2000,
            dec_j2000,
            alt,
            hour_angle,
            lst,
            pier_side,
            tracking: m.properties.get("TELESCOPE_TRACK_STATE").and_then(|p| p.switch("TRACK_ON")),
            parked: m.properties.get("TELESCOPE_PARK").and_then(|p| p.switch("PARK")),
            slewing: eod.state == indi_link::PropertyState::Busy,
        })
    }

    pub(super) fn mount_msg(&self) -> ToUi {
        ToUi::Mount {
            info: self.mount_info().map(Box::new),
        }
    }

    fn mount_name(&self) -> Result<String, String> {
        let name = self.state.roles.mount.clone().ok_or("assign a mount in Equipment first")?;
        let cache = self.client.as_ref().ok_or("not connected to an INDI server")?.cache();
        cache
            .devices
            .get(&name)
            .filter(|d| d.is_connected())
            .ok_or_else(|| format!("mount '{name}' is not connected"))?;
        Ok(name)
    }

    /// Slew to J2000 coordinates and track.
    pub(super) fn mount_goto(&mut self, ra_hours: f64, dec_deg: f64) -> Result<(), String> {
        if !(0.0..24.0).contains(&ra_hours) || !(-90.0..=90.0).contains(&dec_deg) {
            return Err("coordinates out of range".into());
        }
        let name = self.mount_name()?;
        let client = self.client.as_ref().expect("checked in mount_name");
        let (has_j2000, has_coord_set, parked) = {
            let cache = client.cache();
            let m = cache.devices.get(&name).expect("checked");
            (
                m.properties.contains_key("EQUATORIAL_COORD"),
                m.properties.contains_key("ON_COORD_SET"),
                m.properties.get("TELESCOPE_PARK").and_then(|p| p.switch("PARK")) == Some(true),
            )
        };
        if parked {
            return Err("the mount is parked — unpark it first".into());
        }
        let e = |r: Result<(), indi_link::ClientError>| r.map_err(|e| e.to_string());
        if has_coord_set {
            e(client.send_switch(&name, "ON_COORD_SET", &[("TRACK", true)]))?;
        }
        if has_j2000 {
            e(client.send_number(&name, "EQUATORIAL_COORD", &[("RA", ra_hours), ("DEC", dec_deg)]))?;
        } else {
            let jd = astro::julian_day(chrono::Utc::now().timestamp_millis() as f64 / 1000.0);
            let (ra, dec) = astro::precess(ra_hours, dec_deg, astro::J2000, jd);
            e(client.send_number(&name, "EQUATORIAL_EOD_COORD", &[("RA", ra), ("DEC", dec)]))?;
        }
        self.log(
            None,
            format!("Slewing to {} {}", astro::format_ra(ra_hours), astro::format_dec(dec_deg)),
            LogLevel::Info,
        );
        Ok(())
    }

    pub(super) fn mount_park(&mut self, park: bool) -> Result<(), String> {
        let name = self.mount_name()?;
        if park {
            self.guider_stop();
        }
        let el = if park { "PARK" } else { "UNPARK" };
        self.client
            .as_ref()
            .expect("checked")
            .send_switch(&name, "TELESCOPE_PARK", &[(el, true)])
            .map_err(|e| e.to_string())?;
        self.log(None, if park { "Parking mount" } else { "Unparking mount" }, LogLevel::Info);
        Ok(())
    }

    pub(super) fn mount_abort(&mut self) -> Result<(), String> {
        let name = self.mount_name()?;
        self.client
            .as_ref()
            .expect("checked")
            .send_switch(&name, "TELESCOPE_ABORT_MOTION", &[("ABORT", true)])
            .map_err(|e| e.to_string())?;
        self.log(None, "Mount motion aborted", LogLevel::Warning);
        Ok(())
    }

    pub(super) fn mount_tracking(&mut self, on: bool) -> Result<(), String> {
        let name = self.mount_name()?;
        let el = if on { "TRACK_ON" } else { "TRACK_OFF" };
        self.client
            .as_ref()
            .expect("checked")
            .send_switch(&name, "TELESCOPE_TRACK_STATE", &[(el, true)])
            .map_err(|e| e.to_string())
    }

    /// Mount property changed: update UIs, and stop guiding during slews
    /// that the sequencer did not start.
    pub(super) fn mount_on_update(&mut self, device: &str, name: &str) {
        if self.state.roles.mount.as_deref() != Some(device) {
            return;
        }
        if matches!(
            name,
            "EQUATORIAL_EOD_COORD"
                | "EQUATORIAL_COORD"
                | "TELESCOPE_PIER_SIDE"
                | "TELESCOPE_TRACK_STATE"
                | "TELESCOPE_PARK"
                | "GEOGRAPHIC_COORD"
        ) {
            self.send(self.mount_msg());
        }
        if name == "EQUATORIAL_EOD_COORD"
            && self.mount_info().is_some_and(|m| m.slewing)
            && self.guider_is_guiding()
            && !self.sequence_flipping()
            && !self.solving()
        {
            self.guider_stop();
            self.log(None, "Mount is slewing — guiding stopped", LogLevel::Warning);
        }
    }
}
