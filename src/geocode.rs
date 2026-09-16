//! Turning a place name into the location block, and back (DESIGN §5.5).
//!
//! `i l` asks MapKit -- through `assets/geocode.swift`, a sibling of the
//! `reverse-geocode` helper `rename-footage --geocode` uses -- for the place
//! the user typed, and stages what comes back onto the five location fields
//! the way `i u` stages a page's answers: as ordinary edits, undone with `u`,
//! shown in the plan `w` confirms, and never written until then. With no
//! place to look up but coordinates on the file, it runs the other way and
//! names the place the camera recorded.
//!
//! MapKit rather than a web geocoder because the answers then agree with
//! Finder's "Created in Makati" line and with every clip rename-footage has
//! already named, and because it needs no key, no account and no rate-limit
//! etiquette. It does need macOS and the network. The helper is a Swift
//! script compiled on each run: under a second, off the UI thread.
//!
//! Nothing here reads or writes a container.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::model::value::Value;

/// The script, embedded so an installed binary with no `assets/` beside it
/// still has it: written out to the temp directory on first use.
const SCRIPT: &str = include_str!("../assets/geocode.swift");

/// One answer from the geocoder. Any part it had nothing for is empty.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub name: String,
    pub city: String,
    pub state: String,
    pub country: String,
    pub lat: f64,
    pub lon: f64,
}

impl Hit {
    /// The line the picker shows: the place, then where it is.
    pub fn summary(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        for p in [&self.name, &self.city, &self.state, &self.country] {
            if !p.is_empty() && !parts.contains(&p.as_str()) {
                parts.push(p);
            }
        }
        parts.join(", ")
    }

    /// What to stage. The venue name is only an answer to a forward lookup:
    /// asked what is *at* a coordinate, MapKit names the nearest thing it
    /// knows, which for a clip shot in a street is a shop that was not the
    /// subject. A part the geocoder left empty is not staged, so a value the
    /// file already holds there survives.
    pub fn fields(&self, with_name: bool) -> Vec<(&'static str, Value)> {
        let mut out = Vec::new();
        let mut put = |id: &'static str, s: &str| {
            if !s.is_empty() {
                out.push((id, Value::text(s.to_string())));
            }
        };
        if with_name {
            put("location_place", &self.name);
        }
        put("location", &self.city);
        put("location_state", &self.state);
        put("location_country", &self.country);
        put("coordinates", &iso6709(self.lat, self.lon));
        out
    }
}

/// Find places matching a typed description.
pub fn search(query: &str) -> Result<Vec<Hit>> {
    let query = query.trim();
    if query.is_empty() {
        bail!("nothing to look up");
    }
    run(&[query])
}

/// Name the place at a coordinate.
pub fn reverse(lat: f64, lon: f64) -> Result<Vec<Hit>> {
    run(&["--reverse", &lat.to_string(), &lon.to_string()])
}

fn run(args: &[&str]) -> Result<Vec<Hit>> {
    let script = script_path()?;
    let out = Command::new("swift")
        .arg("-suppress-warnings")
        .arg(&script)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running swift (is Xcode or the command line tools installed?)")?;
    let hits = parse(&String::from_utf8_lossy(&out.stdout));
    if hits.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = err.lines().last().map(|l| l.trim_start_matches("geocode: ").trim()).unwrap_or("");
        bail!("{}", if why.is_empty() { "no hits" } else { why });
    }
    Ok(hits)
}

/// The helper's output, one hit per line, six tab-separated columns.
pub fn parse(text: &str) -> Vec<Hit> {
    text.lines()
        .filter_map(|l| {
            let c: Vec<&str> = l.split('\t').collect();
            if c.len() != 6 {
                return None;
            }
            Some(Hit {
                name: c[0].trim().to_string(),
                city: c[1].trim().to_string(),
                state: c[2].trim().to_string(),
                country: c[3].trim().to_string(),
                lat: c[4].trim().parse().ok()?,
                lon: c[5].trim().parse().ok()?,
            })
        })
        .collect()
}

/// The string the container holds: ISO 6709 the way an iPhone writes it, four
/// decimals and a trailing slash, no altitude -- the geocoder has none, and a
/// made-up one would be read as measured.
pub fn iso6709(lat: f64, lon: f64) -> String {
    format!("{lat:+.4}{lon:+.4}/")
}

/// Latitude and longitude out of an ISO 6709 string, `+13.7165+100.5867/` or
/// `+13.7165+100.5867+018.071/`. The altitude, when there is one, is left
/// alone: the reverse lookup does not want it.
pub fn parse_iso6709(s: &str) -> Option<(f64, f64)> {
    let s = s.trim().trim_end_matches('/');
    let mut nums = Vec::new();
    let mut start = None;
    for (i, ch) in s.char_indices() {
        if ch == '+' || ch == '-' {
            if let Some(st) = start {
                nums.push(&s[st..i]);
            }
            start = Some(i);
        }
    }
    if let Some(st) = start {
        nums.push(&s[st..]);
    }
    if nums.len() < 2 {
        return None;
    }
    let lat: f64 = nums[0].parse().ok()?;
    let lon: f64 = nums[1].parse().ok()?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    Some((lat, lon))
}

/// The script on disk: the repo or install copy beside the binary if there is
/// one, else the embedded copy written to the temp directory. Rewritten there
/// whenever it differs, so an upgraded binary never runs last release's copy.
fn script_path() -> Result<PathBuf> {
    let installed = crate::tags::write::asset_path("geocode.swift");
    if installed.exists() {
        return Ok(installed);
    }
    let p = std::env::temp_dir().join("tagform-geocode.swift");
    if std::fs::read_to_string(&p).ok().as_deref() != Some(SCRIPT) {
        std::fs::write(&p, SCRIPT).with_context(|| format!("writing {}", p.display()))?;
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_helpers_columns_and_skips_a_broken_line() {
        let hits = parse("Coro Hotel\tMakati\tMetro Manila\tPhilippines\t14.56410\t121.02995\nbroken line\n");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Coro Hotel");
        assert_eq!(hits[0].country, "Philippines");
        assert_eq!(hits[0].summary(), "Coro Hotel, Makati, Metro Manila, Philippines");
    }

    /// A forward hit stages the venue; a reverse hit deliberately does not.
    #[test]
    fn stages_the_block_and_the_coordinates() {
        let h = parse("Coro Hotel\tMakati\t\tPhilippines\t14.56410\t121.03\n").remove(0);
        let f = h.fields(true);
        assert_eq!(f[0], ("location_place", Value::text("Coro Hotel")));
        assert!(!f.iter().any(|(id, _)| *id == "location_state"), "an empty part is not staged");
        assert_eq!(f.last().unwrap(), &("coordinates", Value::text("+14.5641+121.0300/")));
        assert!(!h.fields(false).iter().any(|(id, _)| *id == "location_place"));
    }

    #[test]
    fn iso6709_round_trips_with_and_without_altitude() {
        assert_eq!(iso6709(14.5641, 121.03), "+14.5641+121.0300/");
        assert_eq!(iso6709(-33.8688, 151.2093), "-33.8688+151.2093/");
        assert_eq!(parse_iso6709("+13.7165+100.5867+018.071/"), Some((13.7165, 100.5867)));
        assert_eq!(parse_iso6709("-33.8688+151.2093/"), Some((-33.8688, 151.2093)));
        assert_eq!(parse_iso6709("Makati"), None);
        assert_eq!(parse_iso6709("+91.0+0.0/"), None);
    }
}
