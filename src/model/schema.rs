//! The field schema (DESIGN §3): what the user sees, and where it lands.
//!
//! A *field* is one label and one control. A *key* is what is stored in the
//! container. The relation is one-to-many — the URL field writes five keys —
//! and that fan-out is the reason this tool exists.
//!
//! The `read` list is deliberately wider than `mdta`: this library has files
//! tagged by several generations of these scripts, so a URL might be present as
//! `comment` (old media-write-tags), `purl` (yt-dlp), `source_url`/`webpage_url`
//! (media-embed) or `original_url` (media-audit). Read accepts any alias; write
//! emits the canonical set. That asymmetry is what makes tagform idempotent.

/// The control a field is edited with (DESIGN §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Control {
    Text,
    TextArea,
    List,
    HashTags,
    Url,
    Stars,
    Enum,
    Date,
    /// Displayed, never edited — probed or write-once data.
    ReadOnly,
}

/// Serialised by `--print-schema`, so the schema is emitted from the one
/// authority rather than transcribed into a document beside it.
#[derive(serde::Serialize)]
pub struct FieldDef {
    pub id: &'static str,
    pub label: &'static str,
    pub control: Control,
    /// Canonical keys written in mdta mode, in order.
    pub mdta: &'static [&'static str],
    /// Atom keys accepted on read, first match wins.
    pub read: &'static [&'static str],
    /// XMP tags, first match wins. Authoritative over atoms when present,
    /// because that is where rename-footage puts authored data (DESIGN §3.6).
    pub xmp: &'static [&'static str],
    /// The iTunes atom, where one exists at all. Measured, not assumed —
    /// see docs/CONTAINER.md.
    pub ilst: Option<&'static str>,
    /// Part of the footage profile: shown only when the value is actually
    /// present in the selection, not gated on any other field (DESIGN §3.6).
    pub footage_only: bool,
    /// Part of the adult-clip profile: shown when the selection agrees it is
    /// an Adult Clip, or when the value is present -- a track number on a
    /// file that is no longer tagged as a clip is still a value, and hiding it
    /// would hide a key the write carries (invariant 4).
    pub clip_only: bool,
    /// Part of the adult profile proper: shown when the selection agrees it
    /// is Adult, whatever the Variant, or when the value is present -- the
    /// same value-keeps-the-row rule as `clip_only`.
    pub adult_only: bool,
    /// The value is a number, so a bare digit in Select mode starts typing it
    /// rather than falling through to a command (DESIGN §5.9). A control flag
    /// would not do: Track is a plain Text control, and the digits mean
    /// something else on the one control that is numeric by construction
    /// (Stars name a rating outright).
    pub numeric: bool,
}

macro_rules! field {
    ($id:literal, $label:literal, $control:expr, mdta: [$($m:literal),*],
     read: [$($r:literal),*], xmp: [$($x:literal),*], ilst: $ilst:expr) => {
        FieldDef {
            id: $id, label: $label, control: $control,
            mdta: &[$($m),*], read: &[$($r),*], xmp: &[$($x),*],
            ilst: $ilst, footage_only: false, clip_only: false, adult_only: false,
            numeric: false,
        }
    };
}

pub static FIELDS: &[FieldDef] = &[
    // What kind of thing the file is: Adult, Footage, Karaoke, Live Visual,
    // Music Video, Tutorial, Meme, Texture. This used to be Genre, which was
    // wrong in both directions -- it is not a style, and it spent the one name
    // every other player already displays as one (DESIGN §3.5). No `read` alias
    // for `genre`: nothing in this library ever carried these values there, so
    // there is nothing to migrate.
    //
    // First, and alone above the rule the form draws under it: it is not one
    // field among the rest but the answer that decides which of the rest are
    // worth showing. Footage wants a location block; a music video wants an
    // artist. `FOOTAGE_*` below is the first profile to cash that promise in.
    //
    // ilst is None because it has not been measured. `catg` exists in the
    // iTunes set, but docs/CONTAINER.md never tested it, and this table only
    // claims atoms that were (invariant 1).
    field!("category", "Category", Control::Enum,
        mdta: ["category"], read: ["category"], xmp: [], ilst: None),

    // Which version of the work this file is: the original, an excerpt, or a
    // remastered/upscaled pass over it. Called `type` on disk until now, which
    // was both too generic to read and a reserved word in every language that
    // touches it -- `Enums.type_` was carrying the trailing underscore. Writes
    // `variant`; still reads `type`, because every file tagged before the
    // rename has one (DESIGN §3.4).
    //
    // Directly under Category, above the same rule: the two closed sets
    // together say what the file is and which version of it this is, and the
    // open fields below the rule describe that thing.
    field!("variant", "Variant", Control::Enum,
        mdta: ["variant"], read: ["variant", "type"], xmp: [], ilst: None),

    field!("title", "Title", Control::Text,
        mdta: ["title"], read: ["title"], xmp: ["XMP-dc:Title"], ilst: Some("\u{a9}nam")),

    // The third closed set, and the first that belongs to one Category alone:
    // Straight, Gay, Sapphic or Trans (`ORIENTATIONS` in config.rs). Only the adult
    // profile offers it unprompted; anywhere else it appears once it holds a
    // value, so the key is never hidden from a write (invariant 4). Its own
    // mdta key, not a tag -- a tag is free text and this is not.
    FieldDef {
        id: "orientation", label: "Orientation", control: Control::Enum,
        mdta: &["orientation"], read: &["orientation"], xmp: &[], ilst: None,
        footage_only: false, clip_only: false, adult_only: true,
        numeric: false,
    },

    // A clip's number within the work it was cut from. Only the adult-clip
    // profile shows it unprompted; anywhere else it appears once it holds a
    // value. `track` under mdta, not the iTunes `trkn` pair -- that atom is a
    // binary (n, total) tuple ffmpeg synthesises from a `track` tag, and it
    // has not been measured (invariant 1).
    //
    // The one numeric field: a clip number is digits and nothing else, so a
    // digit pressed on the row is the value rather than a command.
    FieldDef {
        id: "track", label: "Track", control: Control::Text,
        mdta: &["track"], read: &["track"], xmp: &[], ilst: None,
        footage_only: false, clip_only: true, adult_only: false,
        numeric: true,
    },

    // yt-dlp writes %(cast,uploader)l to both actors and artist; rename-footage
    // writes the same people to XMP as a true list.
    field!("actors", "Actors", Control::List,
        mdta: ["actors", "artist"], read: ["actors", "cast", "artist"],
        xmp: ["XMP-iptcExt:PersonInImage"], ilst: Some("\u{a9}ART")),

    field!("artist", "Artist", Control::Text,
        mdta: ["artist"], read: ["artist"], xmp: [], ilst: Some("\u{a9}ART")),

    // Stars, 0-5. Not rtng, not iTunEXTC (DESIGN §3.3). XMP-xmp:Rating is a real
    // standard 0-5 field and is authoritative wherever it is present.
    field!("rating", "Rating", Control::Stars,
        mdta: ["rating"], read: ["rating"], xmp: ["XMP-xmp:Rating"], ilst: None),

    field!("description", "Description", Control::TextArea,
        mdta: ["description"], read: ["description"],
        xmp: ["XMP-dc:Description"], ilst: Some("desc")),

    // One field, five keys.
    field!("url", "URL", Control::Url,
        mdta: ["webpage_url", "source_url", "purl", "comment", "original_url"],
        read: ["webpage_url", "source_url", "purl", "original_url", "comment"],
        xmp: [], ilst: Some("purl")),

    field!("channel", "Channel", Control::Text,
        mdta: ["channel", "album_artist", "album"],
        read: ["channel", "album_artist", "album"],
        xmp: ["XMP-xmpDM:Album"], ilst: Some("aART")),

    field!("tags", "Tags", Control::HashTags,
        mdta: ["keywords"], read: ["keywords", "keyw"],
        xmp: ["XMP-dc:Subject"], ilst: Some("keyw")),

    // The real one now: an open text field for the musical or cinematic style,
    // which is what `genre`/`©gen` means to Plex, Jellyfin, Music.app and
    // Finder. No enum -- a style list is not a closed set, and the closed set
    // that used to live here moved to Category above.
    field!("genre", "Genre", Control::Text,
        mdta: ["genre"], read: ["genre"], xmp: [], ilst: Some("\u{a9}gen")),

    // The iTunes media kind (stik).
    field!("kind", "Kind", Control::Enum,
        mdta: ["media_type"], read: ["media_type"], xmp: [], ilst: Some("stik")),

    // `com.apple.quicktime.creationdate` is what an iPhone writes: a real
    // authored capture time. Without it a camera clip showed "Date —" while its
    // actual date sat in the Custom section a few rows below.
    //
    // `creation_time` is the `mvhd` creation time, which ffprobe reports under
    // that name. It used to be excluded as muxer bookkeeping on the theory
    // that every file would carry a date nobody set -- but a plain ffmpeg mux
    // leaves it at zero (ffprobe then omits it), so when it is present someone
    // authored it. An Android camera, and most cameras that are not iPhones,
    // store the capture time *only* there, so without it every such clip read
    // "Date —" while mediainfo showed the date plainly. It reads last, so an
    // authored `date` or a phone's `creationdate` wins; it is never written,
    // and the remux takes care to keep it (write.rs, atoms::restore_times).
    field!("date", "Date", Control::Date,
        mdta: ["date"],
        read: ["date", "com.apple.quicktime.creationdate", "creation_time"],
        xmp: ["XMP-xmp:CreateDate"], ilst: Some("\u{a9}day")),

    field!("synopsis", "Synopsis", Control::TextArea,
        mdta: ["synopsis"], read: ["synopsis"], xmp: [], ilst: Some("ldes")),

    field!("origin", "Origin", Control::Text,
        mdta: ["origin"], read: ["origin"], xmp: [], ilst: None),

    // The venue: "Coro Hotel", where Location below is "Makati". The one
    // location row that is always in the form, because it is where a place
    // is typed: committing text here runs the MapKit lookup (src/geocode.rs),
    // which rewrites it to what MapKit calls the place and fills the four
    // rows below from the hit. The one part of the block a reverse lookup
    // leaves alone -- asked what is at a coordinate, the geocoder names the
    // nearest thing it knows, not the subject. IPTC's sublocation.
    FieldDef {
        id: "location_place", label: "Place", control: Control::Text,
        mdta: &[], read: &[],
        xmp: &["XMP-iptcExt:LocationCreatedSublocation"], ilst: None, footage_only: false, clip_only: false, adult_only: false,
        numeric: false,
    },
    // A city name, and only that. It deliberately does NOT read the `location`
    // atom, which is where ffmpeg puts a coordinate string: the field showed
    // "+13.7165+100.5867+018.071/" as though it were a city -- and an edit
    // would have written a place name into a coordinate. The numbers get
    // their own field below.
    FieldDef {
        id: "location", label: "Location", control: Control::Text,
        mdta: &[], read: &[],
        xmp: &["XMP-iptcExt:LocationCreatedCity"], ilst: None, footage_only: true, clip_only: false, adult_only: false,
        numeric: false,
    },
    // The whole ISO 6709 string, which is what the container actually holds,
    // under the key an iPhone writes and Finder reads. Written by the camera,
    // or by the `i l` lookup from the place typed above; rename-footage
    // --geocode turns it back into the place name. ffprobe reports the Apple
    // key verbatim (lower-cased on read, like every probed name); `location`
    // and `location-eng` are where a file that has been through ffmpeg
    // without `use_metadata_tags` carries the same string, language-tagged
    // from its udta copy. The XMP latitude is deliberately not read here: on
    // its own it is half a coordinate, and it shows up in the Custom group
    // alongside its longitude.
    FieldDef {
        id: "coordinates", label: "Coordinates", control: Control::Text,
        mdta: &["com.apple.quicktime.location.ISO6709"],
        read: &["com.apple.quicktime.location.iso6709", "location", "location-eng"],
        xmp: &[], ilst: None, footage_only: true, clip_only: false, adult_only: false,
        numeric: false,
    },
    // Write-once: the only surviving copy of a camera's own IMG_4855.MOV.
    // rename-footage --geocode writes the city as one field of an IPTC block and
    // fills in the rest alongside it, deliberately: "the plain-text place and
    // the numbers it came from end up in the same structure". Editing the city
    // without seeing the province and country next to it is how they drift apart.
    FieldDef {
        id: "location_state", label: "State", control: Control::Text,
        mdta: &[], read: &[],
        xmp: &["XMP-iptcExt:LocationCreatedProvinceState"], ilst: None, footage_only: true, clip_only: false, adult_only: false,
        numeric: false,
    },
    FieldDef {
        id: "location_country", label: "Country", control: Control::Text,
        mdta: &[], read: &[],
        xmp: &["XMP-iptcExt:LocationCreatedCountryName"], ilst: None, footage_only: true, clip_only: false, adult_only: false,
        numeric: false,
    },
    FieldDef {
        id: "preserved_name", label: "Original name", control: Control::ReadOnly,
        mdta: &[], read: &[], xmp: &["XMP-xmpMM:PreservedFileName"],
        ilst: None, footage_only: true, clip_only: false, adult_only: false,
        numeric: false,
    },
];

/// The Category whose profile reshapes the form (§3.6).
pub const FOOTAGE: &str = "Footage";

/// Fields a footage clip has no use for. A camera file has no artist, no
/// channel and no URL -- it was not published anywhere -- and its one prose
/// field is Description. Hiding them is display only: an unshown key is still
/// read, still carried in the report, and still written back untouched
/// (invariant 4).
pub static FOOTAGE_HIDDEN: &[&str] = &["artist", "url", "channel", "synopsis"];

/// The order a footage clip is filled in: what it is, then when, then who,
/// then how good, then how to find it again, then where -- and only after
/// all that the prose, which is the part most clips never get. The remaining
/// fields keep their schema order below these.
pub static FOOTAGE_ORDER: &[&str] =
    &["category", "variant", "date", "actors", "rating", "tags", "location_place", "title", "description"];

/// Where a footage clip wears a different name. `actors` is the container key
/// and stays one -- yt-dlp's cast list lands there -- but nobody filming a
/// street calls the people in it actors.
pub fn footage_label(id: &str) -> Option<&'static str> {
    (id == "actors").then_some("People")
}

/// Position in `FOOTAGE_ORDER`; see `profile_rank`.
pub fn footage_rank(id: &str) -> usize {
    profile_rank(FOOTAGE_ORDER, id)
}

/// The second Category with a profile of its own (§3.6).
pub const ADULT: &str = "Adult";

/// The Variant that adds a track number to an adult file: a clip is one cut
/// of a longer work, and the number says which.
pub const CLIP: &str = "Clip";

/// An adult file is published under a channel and credits its actors; there
/// is no separate artist, and no place it was shot that anyone types in.
/// Same display-only rule as `FOOTAGE_HIDDEN`.
pub static ADULT_HIDDEN: &[&str] = &["artist", "location_place"];

/// The order an adult file is filled in. Orientation sits with the other two
/// closed sets, because it is one; Track sits with Title because it
/// qualifies it -- "this work, cut N". Kind and the footage fields are not
/// named and keep schema order behind these.
pub static ADULT_ORDER: &[&str] = &[
    "category", "variant", "orientation", "title", "track", "channel", "actors", "rating",
    "url", "tags", "date", "description", "genre", "synopsis", "origin",
];

/// Position in a profile's order, or past its end for a field it does not
/// name. Sorting by this and nothing else keeps the unnamed fields in schema
/// order, because the sort is stable.
pub fn profile_rank(order: &[&str], id: &str) -> usize {
    order.iter().position(|f| *f == id).unwrap_or(order.len())
}

/// Muxer bookkeeping. Hidden from the form, and actively cleared on write —
/// with `-map_metadata 0` plus `use_metadata_tags`, ffmpeg promotes these to
/// real readable tags that then accumulate on every rewrite (docs/CONTAINER.md).
///
/// `creation_time` is not here: it is the container's capture time and the
/// Date field reads it. The remux still clears it from ffmpeg's metadata
/// dictionary, for the same accumulation reason, and restores it into `mvhd`
/// afterwards (plan::junk_clears).
pub static JUNK_KEYS: &[&str] = &[
    "major_brand", "minor_version", "compatible_brands", "encoder",
    "handler_name", "vendor_id",
];

/// Every XMP tag any field claims, for splitting known from custom. Without
/// this, XMP tags no field knows about are invisible: they survive a write, but
/// nothing shows they are there.
pub fn claimed_xmp_tags() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = FIELDS.iter().flat_map(|f| f.xmp.iter().copied()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Every atom key any field claims, for splitting known from custom.
pub fn field_by_id(id: &str) -> Option<&'static FieldDef> {
    FIELDS.iter().find(|f| f.id == id)
}

pub fn claimed_atom_keys() -> Vec<&'static str> {
    let mut v: Vec<&'static str> =
        FIELDS.iter().flat_map(|f| f.read.iter().chain(f.mdta.iter()).copied()).collect();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The split Category made: the closed set is Category's, and `genre` is
    /// free text on the key every other player reads as a style. Neither reads
    /// the other -- there was never a file carrying a category under `genre`.
    #[test]
    fn category_holds_the_set_and_genre_is_free_text() {
        let cat = field_by_id("category").expect("category field");
        let genre = field_by_id("genre").expect("genre field");
        assert_eq!(cat.control, Control::Enum);
        assert_eq!(genre.control, Control::Text);
        assert_eq!(cat.mdta, ["category"]);
        assert_eq!(genre.mdta, ["genre"]);
        assert!(!cat.read.contains(&"genre"));
        assert!(!genre.read.contains(&"category"));
    }

    /// The order is load-bearing, not cosmetic: Category is the answer the rest
    /// of the form will be filtered by, and the renderer draws its group rule
    /// by finding it. A field inserted above it would silently take both.
    #[test]
    fn category_leads_the_form() {
        assert_eq!(FIELDS[0].id, "category");
    }

    /// Variant sits directly under Category, above the rule the form draws
    /// under the pair: the two closed sets say what the file is, and the
    /// open fields below describe it.
    #[test]
    fn variant_follows_category() {
        assert_eq!(FIELDS[1].id, "variant");
        assert_eq!(FIELDS[2].id, "title");
    }

    /// Same check as the footage profile: a name that is not a field would
    /// hide nothing and order nothing, silently.
    #[test]
    fn the_adult_profile_names_only_real_fields() {
        for id in ADULT_HIDDEN.iter().chain(ADULT_ORDER) {
            assert!(field_by_id(id).is_some(), "{id} is not a field");
        }
        for id in ADULT_HIDDEN {
            assert!(!ADULT_ORDER.contains(id), "{id} is both hidden and ordered");
        }
        assert_eq!(ADULT_ORDER[..2], ["category", "variant"]);
        assert!(ADULT_ORDER.contains(&"track"));
        assert!(field_by_id("track").unwrap().clip_only);
        assert!(ADULT_ORDER.contains(&"orientation"));
        let orientation = field_by_id("orientation").unwrap();
        assert!(orientation.adult_only && !orientation.clip_only);
        assert_eq!(orientation.control, Control::Enum);
        assert!(profile_rank(ADULT_ORDER, "kind") > profile_rank(ADULT_ORDER, "origin"));
    }

    /// Every id the footage profile names has to be a real field, or the
    /// profile would silently hide nothing and order nothing.
    #[test]
    fn the_footage_profile_names_only_real_fields() {
        for id in FOOTAGE_HIDDEN.iter().chain(FOOTAGE_ORDER) {
            assert!(field_by_id(id).is_some(), "{id} is not a field");
        }
        for id in FOOTAGE_HIDDEN {
            assert!(!FOOTAGE_ORDER.contains(id), "{id} is both hidden and ordered");
        }
        assert_eq!(FOOTAGE_ORDER[0], "category", "the answer that picks the profile leads it");
        assert!(FOOTAGE_HIDDEN.contains(&"artist"));
    }

    /// The rank is what the sort reads, and an unnamed field must land after
    /// every named one rather than tying with the first.
    #[test]
    fn unnamed_fields_rank_after_the_ordered_ones() {
        assert_eq!(footage_rank("category"), 0);
        assert_eq!(footage_rank("description"), FOOTAGE_ORDER.len() - 1);
        assert!(footage_rank("genre") > footage_rank("description"));
        assert_eq!(footage_rank("genre"), footage_rank("origin"), "ties keep schema order");
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<_> = FIELDS.iter().map(|f| f.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate field id");
    }

    /// Every canonical write key must also be readable, or a value tagform
    /// wrote would not be seen on the next open.
    #[test]
    fn write_keys_round_trip_through_read() {
        for f in FIELDS {
            for k in f.mdta {
                // Probed names are lower-cased, so the read list holds the
                // lower-cased spelling of a reverse-DNS key.
                let read = k.to_ascii_lowercase();
                assert!(f.read.contains(&read.as_str()), "{}: writes {k} but cannot read it", f.id);
            }
        }
    }

    #[test]
    fn junk_keys_are_not_claimed_by_any_field() {
        let claimed = claimed_atom_keys();
        for j in JUNK_KEYS {
            assert!(!claimed.contains(j), "{j} is both junk and a field key");
        }
    }
}
