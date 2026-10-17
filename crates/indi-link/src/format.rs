//! Formatting and parsing of INDI number values.
//!
//! INDI number elements carry a printf-style format string. In addition to the
//! usual `%f`, `%e`, `%g` and `%d`, INDI defines `%<w>.<f>m` for sexagesimal
//! output (e.g. `%010.6m` renders hours as `hh:mm:ss`).

/// Format `value` according to an INDI printf-style `format`.
///
/// Unknown or malformed formats fall back to Rust's shortest representation.
pub fn format_number(value: f64, format: &str) -> String {
    let Some(spec) = Spec::parse(format) else {
        return value.to_string();
    };
    let body = match spec.conv {
        'f' | 'F' => format!("{:.*}", spec.precision.unwrap_or(6), value),
        'e' | 'E' => format_exp(value, spec.precision.unwrap_or(6), spec.conv == 'E'),
        'g' | 'G' => format_general(value, spec.precision.unwrap_or(6), spec.alt),
        'd' | 'i' | 'u' => format!("{}", value.round() as i64),
        'x' => format!("{:x}", value.round() as i64),
        'm' => format_sexa(value, spec.width.unwrap_or(0), spec.precision.unwrap_or(0)),
        _ => value.to_string(),
    };
    let body = if spec.plus && value >= 0.0 && !body.starts_with('-') {
        format!("+{body}")
    } else {
        body
    };
    match spec.width {
        // `%m` already handles its own width.
        Some(w) if spec.conv != 'm' && body.len() < w => {
            if spec.left {
                format!("{body:<w$}")
            } else if spec.zero && !body.starts_with('-') {
                format!("{body:0>w$}")
            } else {
                format!("{body:>w$}")
            }
        }
        _ => body,
    }
}

/// Parse a number as typed by a user or sent by a driver: decimal (`12.5`,
/// `-1e3`) or sexagesimal (`12:30:15`, `-05 30 00.5`, `12:30.5`).
pub fn parse_number(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(v) = s.parse::<f64>() {
        return Some(v);
    }
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let parts: Vec<&str> = rest
        .split(|c: char| c == ':' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    let mut scale = 1.0;
    for p in parts {
        let v: f64 = p.parse().ok()?;
        if v < 0.0 {
            return None;
        }
        total += v / scale;
        scale *= 60.0;
    }
    Some(if neg { -total } else { total })
}

struct Spec {
    left: bool,
    plus: bool,
    zero: bool,
    alt: bool,
    width: Option<usize>,
    precision: Option<usize>,
    conv: char,
}

impl Spec {
    fn parse(fmt: &str) -> Option<Self> {
        let mut chars = fmt.trim().strip_prefix('%')?.chars().peekable();
        let mut spec = Spec {
            left: false,
            plus: false,
            zero: false,
            alt: false,
            width: None,
            precision: None,
            conv: 'g',
        };
        while let Some(&c) = chars.peek() {
            match c {
                '-' => spec.left = true,
                '+' => spec.plus = true,
                '0' => spec.zero = true,
                '#' => spec.alt = true,
                ' ' => {}
                _ => break,
            }
            chars.next();
        }
        let digits = |chars: &mut std::iter::Peekable<std::str::Chars>| {
            let mut n: Option<usize> = None;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                n = Some(n.unwrap_or(0) * 10 + d as usize);
                chars.next();
            }
            n
        };
        spec.width = digits(&mut chars);
        if chars.peek() == Some(&'.') {
            chars.next();
            spec.precision = Some(digits(&mut chars).unwrap_or(0));
        }
        while matches!(chars.peek(), Some('l' | 'h' | 'L')) {
            chars.next();
        }
        spec.conv = chars.next()?;
        Some(spec)
    }
}

fn format_exp(value: f64, precision: usize, upper: bool) -> String {
    // Rust renders `1.5e3`; C renders `1.500000e+03`.
    let s = format!("{:.*e}", precision, value);
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let e = if upper { 'E' } else { 'e' };
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}{e}{sign}{:02}", exp.abs())
}

fn format_general(value: f64, precision: usize, alt: bool) -> String {
    if value == 0.0 || !value.is_finite() {
        return if value == 0.0 { "0".into() } else { value.to_string() };
    }
    let p = precision.max(1);
    // Exponent after rounding to p significant digits.
    let rounded = format!("{:.*e}", p - 1, value);
    let exp: i32 = rounded
        .split_once('e')
        .and_then(|(_, e)| e.parse().ok())
        .unwrap_or(0);
    let s = if exp < -4 || exp >= p as i32 {
        format_exp(value, p - 1, false)
    } else {
        format!("{:.*}", (p as i32 - 1 - exp).max(0) as usize, value)
    };
    if alt { s } else { strip_trailing_zeros(&s) }
}

fn strip_trailing_zeros(s: &str) -> String {
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => s.split_at(i),
        None => (s, ""),
    };
    let mantissa = if mantissa.contains('.') {
        mantissa.trim_end_matches('0').trim_end_matches('.')
    } else {
        mantissa
    };
    format!("{mantissa}{exp}")
}

/// libindi `numberFormat`/`fs_sexa` semantics: `f` selects the fraction
/// resolution, `w - f` is the width of the leading (degrees/hours) field.
fn format_sexa(value: f64, width: usize, f: usize) -> String {
    let fracbase: u64 = match f {
        9 => 360_000,
        8 => 36_000,
        6 => 3_600,
        5 => 600,
        _ => 60,
    };
    let neg = value < 0.0;
    let n = (value.abs() * fracbase as f64).round() as u64;
    let d = n / fracbase;
    let frac = n % fracbase;
    let lead = format!("{}{}", if neg { "-" } else { "" }, d);
    let lead_width = width.saturating_sub(f);
    let lead = format!("{lead:>lead_width$}");
    match fracbase {
        60 => format!("{lead}:{:02}", frac),
        600 => format!("{lead}:{:02}.{}", frac / 10, frac % 10),
        3_600 => format!("{lead}:{:02}:{:02}", frac / 60, frac % 60),
        36_000 => format!(
            "{lead}:{:02}:{:02}.{}",
            frac / 600,
            (frac % 600) / 10,
            frac % 10
        ),
        _ => format!(
            "{lead}:{:02}:{:02}.{:02}",
            frac / 6000,
            (frac % 6000) / 100,
            frac % 100
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printf_basics() {
        assert_eq!(format_number(1.23456, "%.2f"), "1.23");
        assert_eq!(format_number(3.0, "%6.1f"), "   3.0");
        assert_eq!(format_number(3.0, "%-6.1f"), "3.0   ");
        assert_eq!(format_number(42.4, "%d"), "42");
        assert_eq!(format_number(1500.0, "%.2e"), "1.50e+03");
        assert_eq!(format_number(0.000123, "%g"), "0.000123");
        assert_eq!(format_number(1234567.0, "%g"), "1.23457e+06");
        assert_eq!(format_number(100.0, "%g"), "100");
        assert_eq!(format_number(5.0, "%05.1f"), "005.0");
        assert_eq!(format_number(2.5, "garbage"), "2.5");
    }

    #[test]
    fn sexagesimal() {
        assert_eq!(format_number(12.5042, "%010.6m"), "  12:30:15");
        assert_eq!(format_number(-5.5, "%9.6m"), " -5:30:00");
        assert_eq!(format_number(1.0 + 2.0 / 60.0 + 3.25 / 3600.0, "%.8m"), "1:02:03.3");
        assert_eq!(format_number(1.0 + 2.0 / 60.0 + 3.25 / 3600.0, "%.9m"), "1:02:03.25");
        assert_eq!(format_number(10.25, "%.5m"), "10:15.0");
        assert_eq!(format_number(10.25, "%.3m"), "10:15");
        // Rounding carries into minutes.
        assert_eq!(format_number(0.999999, "%.6m"), "1:00:00");
    }

    #[test]
    fn parse_values() {
        assert_eq!(parse_number(" 12.5 "), Some(12.5));
        assert_eq!(parse_number("-1e3"), Some(-1000.0));
        assert_eq!(parse_number("12:30:00"), Some(12.5));
        assert_eq!(parse_number("-05 30 00"), Some(-5.5));
        assert_eq!(parse_number("+10:15"), Some(10.25));
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("abc"), None);
        assert_eq!(parse_number("1:-2"), None);
    }
}
