//! Positional astronomy needed for FITS metadata (Meeus, *Astronomical Algorithms*).

pub const J2000: f64 = 2_451_545.0;

pub fn julian_day(unix_seconds: f64) -> f64 {
    unix_seconds / 86_400.0 + 2_440_587.5
}

/// Greenwich mean sidereal time in degrees (Meeus 12.4).
pub fn gmst_deg(jd: f64) -> f64 {
    let t = (jd - J2000) / 36_525.0;
    (280.460_618_37 + 360.985_647_366_29 * (jd - J2000) + 0.000_387_933 * t * t
        - t * t * t / 38_710_000.0)
        .rem_euclid(360.0)
}

/// Altitude in degrees. Longitude is east-positive.
pub fn altitude_deg(ra_hours: f64, dec_deg: f64, lat_deg: f64, lon_east_deg: f64, jd: f64) -> f64 {
    let lst = gmst_deg(jd) + lon_east_deg;
    let ha = (lst - ra_hours * 15.0).to_radians();
    let (dec, lat) = (dec_deg.to_radians(), lat_deg.to_radians());
    (lat.sin() * dec.sin() + lat.cos() * dec.cos() * ha.cos())
        .asin()
        .to_degrees()
}

/// Pickering (2002) airmass; `None` below the horizon.
pub fn airmass(alt_deg: f64) -> Option<f64> {
    (alt_deg > 0.0).then(|| {
        let h = alt_deg;
        1.0 / (h + 244.0 / (165.0 + 47.0 * h.powf(1.1))).to_radians().sin()
    })
}

/// Precess equatorial coordinates between epochs (Meeus 21.2–21.4).
pub fn precess(ra_hours: f64, dec_deg: f64, jd_from: f64, jd_to: f64) -> (f64, f64) {
    let big_t = (jd_from - J2000) / 36_525.0;
    let t = (jd_to - jd_from) / 36_525.0;
    let arcsec = |v: f64| (v / 3600.0).to_radians();
    let base = 2306.2181 + 1.396_56 * big_t - 0.000_139 * big_t * big_t;
    let zeta = arcsec(base * t + (0.30188 - 0.000_344 * big_t) * t * t + 0.017_998 * t * t * t);
    let z = arcsec(base * t + (1.09468 + 0.000_066 * big_t) * t * t + 0.018_203 * t * t * t);
    let theta = arcsec(
        (2004.3109 - 0.853_30 * big_t - 0.000_217 * big_t * big_t) * t
            - (0.42665 + 0.000_217 * big_t) * t * t
            - 0.041_833 * t * t * t,
    );
    let (ra, dec) = ((ra_hours * 15.0).to_radians(), dec_deg.to_radians());
    let a = dec.cos() * (ra + zeta).sin();
    let b = theta.cos() * dec.cos() * (ra + zeta).cos() - theta.sin() * dec.sin();
    let c = theta.sin() * dec.cos() * (ra + zeta).cos() + theta.cos() * dec.sin();
    let ra_out = (a.atan2(b) + z).to_degrees().rem_euclid(360.0) / 15.0;
    (ra_out, c.clamp(-1.0, 1.0).asin().to_degrees())
}

/// `HH MM SS.ss`
pub fn format_ra(hours: f64) -> String {
    let total = (hours.rem_euclid(24.0) * 360_000.0).round() as u64 % (24 * 360_000);
    format!(
        "{:02} {:02} {:02}.{:02}",
        total / 360_000,
        total / 6_000 % 60,
        total / 100 % 60,
        total % 100
    )
}

/// `+DD MM SS.s`
pub fn format_dec(deg: f64) -> String {
    let sign = if deg < 0.0 { '-' } else { '+' };
    let total = (deg.abs() * 36_000.0).round() as u64;
    format!(
        "{sign}{:02} {:02} {:02}.{}",
        total / 36_000,
        total / 600 % 60,
        total / 10 % 60,
        total % 10
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidereal_time_meeus_12a() {
        // 1987 April 10, 0h UT → 13h10m46.3668s
        let expected = (13.0 + 10.0 / 60.0 + 46.3668 / 3600.0) * 15.0;
        assert!((gmst_deg(2_446_895.5) - expected).abs() < 1e-5);
    }

    #[test]
    fn precession_meeus_21b() {
        // θ Persei (with proper motion applied) J2000 → JD 2462088.69
        let (ra, dec) = precess(41.054_063 / 15.0, 49.227_750, J2000, 2_462_088.69);
        assert!((ra * 15.0 - 41.547_214).abs() < 2e-6, "{}", ra * 15.0);
        assert!((dec - 49.348_483).abs() < 2e-6, "{dec}");
        // And back again.
        let (ra0, dec0) = precess(ra, dec, 2_462_088.69, J2000);
        assert!((ra0 * 15.0 - 41.054_063).abs() < 1e-5);
        assert!((dec0 - 49.227_750).abs() < 1e-5);
    }

    #[test]
    fn altitude_and_airmass() {
        let jd = 2_460_000.25;
        let lst_h = gmst_deg(jd) / 15.0 + 15.0 / 15.0;
        // Object on the meridian at dec = latitude is at the zenith.
        // asin() is ill-conditioned at 90°, so allow a little slack.
        assert!((altitude_deg(lst_h, 46.0, 46.0, 15.0, jd) - 90.0).abs() < 1e-4);
        assert!((altitude_deg(lst_h + 12.0, 0.0, 46.0, 15.0, jd) + 44.0).abs() < 1e-6);
        assert!((airmass(90.0).unwrap() - 1.0).abs() < 1e-3);
        assert!((airmass(30.0).unwrap() - 2.0).abs() < 0.01);
        assert_eq!(airmass(-1.0), None);
    }

    #[test]
    fn sexagesimal_strings() {
        assert_eq!(format_ra(5.5916), "05 35 29.76");
        assert_eq!(format_ra(23.999_999_9), "00 00 00.00");
        assert_eq!(format_dec(-5.3911), "-05 23 28.0");
        assert_eq!(format_dec(41.269), "+41 16 08.4");
    }
}
