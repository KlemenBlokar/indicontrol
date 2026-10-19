//! Deep-sky object lookup in the embedded OpenNGC catalogue (NGC, IC,
//! Messier and some extras), by designation or common name.

use std::collections::HashMap;
use std::sync::OnceLock;

use indi_link::parse_number;
use protocol::CatalogObject;

const NGC_CSV: &str = include_str!("../data/openngc/NGC.csv");
const ADDENDUM_CSV: &str = include_str!("../data/openngc/addendum.csv");

// Columns of the OpenNGC CSV files.
const NAME: usize = 0;
const TYPE: usize = 1;
const RA: usize = 2;
const DEC: usize = 3;
const B_MAG: usize = 8;
const V_MAG: usize = 9;
const MESSIER: usize = 23;
const NGC: usize = 24;
const IC: usize = 25;
const IDENTIFIERS: usize = 27;
const COMMON_NAMES: usize = 28;

struct Entry {
    object: CatalogObject,
    /// Normalized designations, see [`key`].
    keys: Vec<String>,
    /// Each common name as a [`key`] and as lowercase words.
    common_keys: Vec<String>,
    common_words: Vec<Vec<String>>,
}

/// Up to `limit` objects matching `query`, best first.
///
/// Exact designations ("M110", "ngc 205", "NGC0205") come first, then exact
/// common names, designations starting with the query ("M4" → M 4, M 40…)
/// and common names containing words starting with every query word
/// ("orion neb" → Great Orion Nebula).
pub fn search(query: &str, limit: usize) -> Vec<CatalogObject> {
    let q = key(query);
    let words = words(query);
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(Score, &Entry)> = catalog()
        .iter()
        .filter_map(|e| score(e, &q, &words).map(|s| (s, e)))
        .collect();
    hits.sort_by(|(a, _), (b, _)| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(b.2)));
    hits.into_iter().take(limit).map(|(_, e)| e.object.clone()).collect()
}

/// Lower is better: match kind, then tie-breaks.
type Score<'a> = (u8, f32, &'a str);

fn score<'a>(e: &'a Entry, q: &str, words: &[String]) -> Option<Score<'a>> {
    if e.keys.iter().any(|k| k == q) {
        return Some((0, 0.0, ""));
    }
    if e.common_keys.iter().any(|k| k == q) {
        return Some((1, 0.0, ""));
    }
    // In designation order: M 4, then M 40, M 41…
    if let Some(k) = e.keys.iter().filter(|k| k.starts_with(q)).min_by_key(|k| (k.len(), k.as_str())) {
        return Some((2, k.len() as f32, k));
    }
    let prefixes = |name: &[String]| words.iter().all(|q| name.iter().any(|w| w.starts_with(q.as_str())));
    if !words.is_empty() && e.common_words.iter().any(|w| prefixes(w)) {
        // Brightest first.
        return Some((3, e.object.mag.unwrap_or(99.0), &e.object.name));
    }
    None
}

fn catalog() -> &'static [Entry] {
    static CATALOG: OnceLock<Vec<Entry>> = OnceLock::new();
    CATALOG.get_or_init(|| parse(&[NGC_CSV, ADDENDUM_CSV]))
}

fn parse(files: &[&str]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut by_name = HashMap::new();
    let mut by_messier = HashMap::new();
    // Duplicate entries only point to the main entry (NGC, IC or M column).
    let mut dups = Vec::new();
    for line in files.iter().flat_map(|f| f.lines().skip(1)) {
        let cols: Vec<&str> = line.split(';').collect();
        if cols.len() <= COMMON_NAMES {
            continue;
        }
        match cols[TYPE] {
            "NonEx" => continue,
            "Dup" => {
                dups.push(cols);
                continue;
            }
            _ => {}
        }
        if let Some(e) = entry(&cols) {
            by_name.insert(cols[NAME].to_owned(), entries.len());
            let messier = strip_zeros(cols[MESSIER].trim());
            if !messier.is_empty() {
                by_messier.insert(messier.to_owned(), entries.len());
            }
            entries.push(e);
        }
    }
    for cols in dups {
        let messier = strip_zeros(cols[MESSIER].trim());
        // M 102 is only listed as a duplicate of M 101 (controversial).
        let master = cross_refs(&cols)
            .find_map(|(name, _)| by_name.get(&name).copied())
            .or_else(|| by_messier.get(messier).copied());
        if let Some(i) = master {
            let e = &mut entries[i];
            e.keys.push(key(cols[NAME]));
            if !messier.is_empty() && !e.object.name.starts_with("M ") {
                let m = format!("M {messier}");
                e.keys.push(key(&m));
                let old = std::mem::replace(&mut e.object.name, m);
                e.object.aliases.insert(0, old);
            }
        }
    }
    entries
}

fn entry(cols: &[&str]) -> Option<Entry> {
    let ra_hours = parse_number(cols[RA])?;
    let dec_deg = parse_number(cols[DEC])?;
    let own = pretty(cols[NAME]);
    let messier = cols[MESSIER].trim();
    let mut aliases: Vec<String> = cross_refs(cols).map(|(_, pretty)| pretty).collect();
    let name = if messier.is_empty() {
        own
    } else {
        let m = format!("M {}", strip_zeros(messier));
        // Addendum entries like "M040" have no other name.
        if own != m {
            aliases.insert(0, own);
        }
        m
    };
    let common_names: Vec<String> = split_list(cols[COMMON_NAMES]).map(str::to_owned).collect();
    let mut keys: Vec<String> = std::iter::once(&name).chain(&aliases).map(|n| key(n)).collect();
    keys.extend(split_list(cols[IDENTIFIERS]).map(key));
    let mag = [V_MAG, B_MAG].iter().find_map(|&c| cols[c].trim().parse().ok());
    Some(Entry {
        common_keys: common_names.iter().map(|n| key(n)).collect(),
        common_words: common_names.iter().map(|n| words(n)).collect(),
        keys,
        object: CatalogObject {
            name,
            aliases,
            common_names,
            kind: kind(cols[TYPE]).to_owned(),
            ra_hours,
            dec_deg,
            mag,
        },
    })
}

/// NGC/IC numbers in the cross-reference columns: (CSV name, display name).
fn cross_refs<'a>(cols: &'a [&'a str]) -> impl Iterator<Item = (String, String)> + 'a {
    [(NGC, "NGC"), (IC, "IC")].into_iter().flat_map(move |(col, prefix)| {
        split_list(cols[col]).map(move |n| (format!("{prefix}{n}"), format!("{prefix} {}", strip_zeros(n))))
    })
}

fn split_list(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// "NGC0224" → "NGC 224", "ESO056-115" → "ESO 56-115".
fn pretty(name: &str) -> String {
    match name.find(|c: char| c.is_ascii_digit()) {
        Some(i) if i > 0 => format!("{} {}", &name[..i], strip_zeros(&name[i..])),
        _ => name.to_owned(),
    }
}

/// "0224" → "224", keeping one zero of "0" or "0A".
fn strip_zeros(s: &str) -> &str {
    let t = s.trim_start_matches('0');
    if t.len() < s.len() && !t.starts_with(|c: char| c.is_ascii_digit()) {
        &s[s.len() - t.len() - 1..]
    } else {
        t
    }
}

/// Designation for comparison: lowercase letters and digits only, numbers
/// without leading zeros ("NGC 0224", "ngc224" → "ngc224").
fn key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_number = false;
    for c in s.chars().filter(char::is_ascii_alphanumeric) {
        let digit = c.is_ascii_digit();
        if c == '0' && !in_number {
            continue;
        }
        in_number = digit;
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn kind(code: &str) -> &str {
    match code {
        "*" => "Star",
        "**" => "Double star",
        "*Ass" => "Stellar association",
        "OCl" => "Open cluster",
        "GCl" => "Globular cluster",
        "Cl+N" => "Cluster with nebula",
        "G" => "Galaxy",
        "GPair" => "Galaxy pair",
        "GTrpl" => "Galaxy triplet",
        "GGroup" => "Galaxy group",
        "PN" => "Planetary nebula",
        "HII" => "HII region",
        "DrkN" => "Dark nebula",
        "EmN" => "Emission nebula",
        "Neb" => "Nebula",
        "RfN" => "Reflection nebula",
        "SNR" => "Supernova remnant",
        "Nova" => "Nova",
        other => other,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn names(query: &str) -> Vec<String> {
        search(query, 8).into_iter().map(|o| o.name).collect()
    }

    #[test]
    fn designations() {
        // M 110 is NGC 205, in any spelling.
        for q in ["M110", "m 110", "M-110", "NGC 205", "ngc0205", "NGC205"] {
            let hit = &search(q, 8)[0];
            assert_eq!(hit.name, "M 110", "{q}");
            assert_eq!(hit.aliases, ["NGC 205"]);
            assert_eq!(hit.kind, "Galaxy");
            assert!((hit.ra_hours - (40.0 + 22.08 / 60.0) / 60.0).abs() < 1e-6, "{}", hit.ra_hours);
            assert!((hit.dec_deg - (41.0 + (41.0 + 7.1 / 60.0) / 60.0)).abs() < 1e-6, "{}", hit.dec_deg);
        }
        // Addendum objects.
        assert_eq!(names("M45")[0], "M 45");
        assert_eq!(search("M 45", 1)[0].aliases, ["Mel 22"]);
        assert!(search("M 40", 1)[0].aliases.is_empty());
        assert_eq!(names("B 33")[0], "B 33");
        // Duplicates resolve to the main entry.
        assert_eq!(names("IC 11")[0], "NGC 281");
        assert_eq!(names("M102")[0], "M 101");
        // Other catalogues' identifiers.
        assert_eq!(names("UGC 454")[0], "M 31");
        assert_eq!(names("Sh2-155")[0], "C 9");
        // Every Messier object is there.
        for m in 1..=110 {
            assert!(!search(&format!("M{m}"), 1).is_empty(), "M {m}");
        }
    }

    #[test]
    fn prefixes_in_designation_order() {
        assert_eq!(names("m4")[..4], ["M 4", "M 40", "M 41", "M 42"]);
        assert_eq!(names("NGC 700")[..2], ["NGC 700", "NGC 7000"]);
    }

    #[test]
    fn common_names() {
        assert_eq!(names("Orion Nebula")[0], "M 42");
        assert_eq!(names("orion neb")[0], "M 42");
        assert_eq!(names("andromeda")[0], "M 31");
        assert_eq!(names("pleiades")[0], "M 45");
        assert_eq!(names("Horsehead")[0], "B 33");
        assert_eq!(names("north america")[0], "NGC 7000");
        let veil = names("veil");
        assert!(veil.contains(&"NGC 6960".to_owned()) && veil.contains(&"NGC 6992".to_owned()), "{veil:?}");
    }

    #[test]
    fn no_match() {
        assert!(search("", 8).is_empty());
        assert!(search("  -  ", 8).is_empty());
        assert!(search("xyzzy", 8).is_empty());
        assert_eq!(search("m", 3).len(), 3);
    }

    #[test]
    fn helpers() {
        assert_eq!(pretty("NGC0224"), "NGC 224");
        assert_eq!(pretty("IC1670B"), "IC 1670B");
        assert_eq!(pretty("ESO056-115"), "ESO 56-115");
        assert_eq!(strip_zeros("0"), "0");
        assert_eq!(strip_zeros("00A"), "0A");
        assert_eq!(key("NGC 7000"), "ngc7000");
        assert_eq!(key("SH 2-155"), key("Sh2-155"));
    }
}
