//! Maven version comparison and version-range handling.
//!
//! Implements the subset of the Maven *Dependency Version Requirement
//! Specification* that Curie needs to turn a range such as `[2.9.1,2.11)` into a
//! concrete version when proposing a `curie fetch` fix.  See
//! <https://maven.apache.org/pom.html#Dependency_Version_Requirement_Specification>.
//!
//! [`MavenVersion`] is a pragmatic port of Maven's `ComparableVersion`: enough to
//! order the everyday release/qualifier forms found on Maven Central (including
//! the spec's `2.0-rc1 < 2.0` rule), but not a byte-for-byte reimplementation of
//! every corner of the original algorithm.

use anyhow::{bail, Result};
use std::cmp::Ordering;

/// One token of a parsed version: either a numeric run or a qualifier word.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Int(u64),
    Qual(String),
}

/// A Maven version that can be ordered against other versions.
///
/// `original` is preserved so callers can return the exact string that appeared
/// in `maven-metadata.xml`; ordering is derived purely from `items`.
#[derive(Debug, Clone)]
pub struct MavenVersion {
    original: String,
    items: Vec<Item>,
}

impl MavenVersion {
    pub fn parse(version: &str) -> Self {
        MavenVersion {
            original: version.to_string(),
            items: tokenize(version),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.original
    }
}

/// Split a version into numeric/qualifier items.  `.` and `-` are separators, and
/// a transition between digits and letters also starts a new item, so `rc1`
/// becomes `["rc", "1"]` and `1alpha` becomes `["1", "alpha"]`.
fn tokenize(version: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut token = String::new();
    let mut token_is_digit = false;

    for ch in version.to_ascii_lowercase().chars() {
        if ch == '.' || ch == '-' {
            flush_token(&mut token, &mut items);
            continue;
        }
        let ch_is_digit = ch.is_ascii_digit();
        if !token.is_empty() && ch_is_digit != token_is_digit {
            flush_token(&mut token, &mut items);
        }
        token_is_digit = ch_is_digit;
        token.push(ch);
    }
    flush_token(&mut token, &mut items);
    items
}

/// Push the accumulated token (if any) as an [`Item`] and clear the buffer.
fn flush_token(token: &mut String, items: &mut Vec<Item>) {
    if token.is_empty() {
        return;
    }
    let item = match token.parse::<u64>() {
        Ok(n) => Item::Int(n),
        Err(_) => Item::Qual(normalize_qualifier(token)),
    };
    items.push(item);
    token.clear();
}

/// Collapse Maven's release aliases so they compare equal: `ga`, `final` and
/// `release` all mean "the release" (empty qualifier), and `cr` is `rc`.
fn normalize_qualifier(qualifier: &str) -> String {
    match qualifier {
        "ga" | "final" | "release" => String::new(),
        "cr" => "rc".to_string(),
        other => other.to_string(),
    }
}

/// Sort key for a qualifier, lowest-precedence first:
/// `alpha < beta < milestone < rc < snapshot < "" (release) < sp < unknown`.
/// Unknown qualifiers share the top rank and break ties lexically.
fn qualifier_key(qualifier: &str) -> (u8, &str) {
    let rank = match qualifier {
        "alpha" | "a" => 0,
        "beta" | "b" => 1,
        "milestone" | "m" => 2,
        "rc" => 3,
        "snapshot" => 4,
        "" => 5,
        "sp" => 6,
        _ => 7,
    };
    (rank, qualifier)
}

impl PartialEq for MavenVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for MavenVersion {}

impl PartialOrd for MavenVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MavenVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        // Compare item-by-item, padding the shorter side with a "null" item.
        // A trailing numeric `0` and a trailing release qualifier both compare
        // equal to null, so `1.0 == 1` and `1.0-ga == 1.0`.
        let len = self.items.len().max(other.items.len());
        for i in 0..len {
            let ordering = cmp_item(self.items.get(i), other.items.get(i));
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    }
}

/// Compare two optional items, where `None` is the "null" padding item.
fn cmp_item(a: Option<&Item>, b: Option<&Item>) -> Ordering {
    match (a, b) {
        (Some(Item::Int(x)), Some(Item::Int(y))) => x.cmp(y),
        // An integer item always outranks a qualifier item.
        (Some(Item::Int(_)), Some(Item::Qual(_))) => Ordering::Greater,
        (Some(Item::Qual(_)), Some(Item::Int(_))) => Ordering::Less,
        (Some(Item::Qual(x)), Some(Item::Qual(y))) => qualifier_key(x).cmp(&qualifier_key(y)),
        // Padding: null integer is 0, null qualifier is the release ("").
        (Some(Item::Int(x)), None) => x.cmp(&0),
        (None, Some(Item::Int(y))) => 0.cmp(y),
        (Some(Item::Qual(x)), None) => qualifier_key(x).cmp(&qualifier_key("")),
        (None, Some(Item::Qual(y))) => qualifier_key("").cmp(&qualifier_key(y)),
        (None, None) => Ordering::Equal,
    }
}

/// A single interval within a version range, e.g. `[1.0,2.0)`.
#[derive(Debug, Clone)]
struct Restriction {
    lower: Option<MavenVersion>,
    lower_inclusive: bool,
    upper: Option<MavenVersion>,
    upper_inclusive: bool,
}

impl Restriction {
    fn contains(&self, version: &MavenVersion) -> bool {
        if let Some(lower) = &self.lower {
            let ok = if self.lower_inclusive {
                version >= lower
            } else {
                version > lower
            };
            if !ok {
                return false;
            }
        }
        if let Some(upper) = &self.upper {
            let ok = if self.upper_inclusive {
                version <= upper
            } else {
                version < upper
            };
            if !ok {
                return false;
            }
        }
        true
    }
}

/// A Maven hard-requirement version range: a union of one or more
/// [`Restriction`]s, as written in a POM `<version>` element.
#[derive(Debug, Clone)]
pub struct VersionRange {
    restrictions: Vec<Restriction>,
}

impl VersionRange {
    /// Parse every notation from the spec: `[1.0]`, `(,1.0]`, `[1.2,1.3]`,
    /// `[1.0,2.0)`, `(1.0,2.0]`, `[1.5,)`, and comma-joined unions such as
    /// `(,1.0],[1.2,)` or `(,1.1),(1.1,)`.
    pub fn parse(spec: &str) -> Result<Self> {
        let groups = split_restriction_groups(spec)?;
        if groups.is_empty() {
            bail!("empty version range {:?}", spec);
        }
        let restrictions = groups
            .iter()
            .map(|g| parse_restriction(g, spec))
            .collect::<Result<Vec<_>>>()?;
        Ok(VersionRange { restrictions })
    }

    /// True when `version` satisfies any of this range's restrictions.
    pub fn contains(&self, version: &MavenVersion) -> bool {
        self.restrictions.iter().any(|r| r.contains(version))
    }
}

/// Split a (possibly multi-restriction) range string into its bracketed groups,
/// e.g. `"(,1.0],[1.2,)"` → `["(,1.0]", "[1.2,)"]`.  Commas *inside* a group are
/// left untouched; only the commas separating groups are consumed.
fn split_restriction_groups(spec: &str) -> Result<Vec<String>> {
    let mut groups = Vec::new();
    let mut chars = spec.trim().chars().peekable();

    while let Some(&ch) = chars.peek() {
        match ch {
            ',' | ' ' => {
                chars.next();
            }
            '[' | '(' => {
                let mut group = String::new();
                group.push(chars.next().unwrap());
                let mut closed = false;
                for inner in chars.by_ref() {
                    group.push(inner);
                    if inner == ']' || inner == ')' {
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    bail!("unterminated version range restriction in {:?}", spec);
                }
                groups.push(group);
            }
            _ => bail!("malformed version range {:?}", spec),
        }
    }
    Ok(groups)
}

/// Parse one bracketed group such as `[1.0,2.0)` or `[1.0]` into a [`Restriction`].
fn parse_restriction(group: &str, spec: &str) -> Result<Restriction> {
    let lower_inclusive = group.starts_with('[');
    let upper_inclusive = group.ends_with(']');
    let inner = &group[1..group.len() - 1];

    match inner.split_once(',') {
        // No comma: an exact requirement, e.g. `[1.0]`.
        None => {
            let version = MavenVersion::parse(inner.trim());
            Ok(Restriction {
                lower: Some(version.clone()),
                lower_inclusive: true,
                upper: Some(version),
                upper_inclusive: true,
            })
        }
        // A comma separates the (optional) lower and upper bounds.
        Some((low, high)) => Ok(Restriction {
            lower: bound(low),
            lower_inclusive,
            upper: bound(high),
            upper_inclusive,
        }),
    }
    .map_err(|e: anyhow::Error| e.context(format!("in version range {:?}", spec)))
}

/// An empty bound string means "unbounded"; otherwise parse the version.
fn bound(raw: &str) -> Option<MavenVersion> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(MavenVersion::parse(trimmed))
    }
}

/// Pick the highest of `available` that satisfies **all** of `ranges`, matching
/// Maven's "highest version that satisfies all the hard requirements" rule.
/// Returns the original version string, or `None` if nothing qualifies.
pub fn intersect_highest_satisfying(
    ranges: &[VersionRange],
    available: &[String],
) -> Option<String> {
    available
        .iter()
        .map(|v| MavenVersion::parse(v))
        .filter(|v| ranges.iter().all(|r| r.contains(v)))
        .max()
        .map(|v| v.original)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> MavenVersion {
        MavenVersion::parse(s)
    }

    // ---- ordering ----

    #[test]
    fn numeric_segments_compare_numerically_not_lexically() {
        assert!(v("2.10.1") > v("2.9.1"));
        assert!(v("2.10") > v("2.9"));
    }

    #[test]
    fn trailing_zero_equals_shorter_version() {
        assert_eq!(v("1.0"), v("1"));
        assert_eq!(v("1.0.0"), v("1"));
    }

    #[test]
    fn release_aliases_are_equal() {
        assert_eq!(v("1.0-ga"), v("1.0"));
        assert_eq!(v("1.0-final"), v("1.0"));
        assert_eq!(v("1.0-release"), v("1.0"));
    }

    #[test]
    fn qualifier_precedence_matches_maven() {
        // alpha < beta < milestone < rc < snapshot < release < sp
        assert!(v("1.0-alpha") < v("1.0-beta"));
        assert!(v("1.0-beta") < v("1.0-milestone"));
        assert!(v("1.0-milestone") < v("1.0-rc1"));
        assert!(v("1.0-rc1") < v("1.0-snapshot"));
        assert!(v("1.0-snapshot") < v("1.0"));
        assert!(v("1.0") < v("1.0-sp"));
    }

    #[test]
    fn prerelease_sorts_before_release() {
        // Spec caveat: 2.0-rc1 < 2.0.
        assert!(v("2.0-rc1") < v("2.0"));
        assert!(v("1.0") > v("1.0-snapshot"));
        assert!(v("1.0-snapshot") > v("1.0-rc1"));
    }

    #[test]
    fn cr_is_alias_for_rc() {
        assert_eq!(v("1.0-cr1"), v("1.0-rc1"));
    }

    // ---- range parsing ----

    fn r(spec: &str) -> VersionRange {
        VersionRange::parse(spec).unwrap()
    }

    #[test]
    fn exact_requirement_matches_only_that_version() {
        let range = r("[1.0]");
        assert!(range.contains(&v("1.0")));
        assert!(!range.contains(&v("1.1")));
        assert!(!range.contains(&v("0.9")));
    }

    #[test]
    fn upper_bound_only_inclusive() {
        let range = r("(,1.0]");
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("0.5")));
        assert!(!range.contains(&v("1.1")));
    }

    #[test]
    fn closed_inclusive_range() {
        let range = r("[1.2,1.3]");
        assert!(range.contains(&v("1.2")));
        assert!(range.contains(&v("1.3")));
        assert!(!range.contains(&v("1.4")));
        assert!(!range.contains(&v("1.1")));
    }

    #[test]
    fn half_open_range_excludes_upper() {
        let range = r("[1.0,2.0)");
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("1.9")));
        assert!(!range.contains(&v("2.0")));
        // Spec caveat: a pre-release of the excluded upper bound is included.
        assert!(range.contains(&v("2.0-rc1")));
    }

    #[test]
    fn open_lower_inclusive_upper() {
        let range = r("(1.0,2.0]");
        assert!(!range.contains(&v("1.0")));
        assert!(range.contains(&v("2.0")));
    }

    #[test]
    fn lower_bound_only() {
        let range = r("[1.5,)");
        assert!(range.contains(&v("1.5")));
        assert!(range.contains(&v("9.9")));
        assert!(!range.contains(&v("1.4")));
    }

    #[test]
    fn union_excludes_the_gap() {
        // Any version <= 1.0 OR >= 1.2 (excludes 1.1).
        let range = r("(,1.0],[1.2,)");
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("1.2")));
        assert!(!range.contains(&v("1.1")));
    }

    #[test]
    fn union_excludes_a_single_version() {
        // Any version except 1.1.
        let range = r("(,1.1),(1.1,)");
        assert!(range.contains(&v("1.0")));
        assert!(range.contains(&v("1.2")));
        assert!(!range.contains(&v("1.1")));
    }

    #[test]
    fn malformed_range_is_an_error() {
        assert!(VersionRange::parse("1.0,2.0").is_err());
        assert!(VersionRange::parse("[1.0,2.0").is_err());
        assert!(VersionRange::parse("").is_err());
    }

    // ---- selection ----

    #[test]
    fn highest_satisfying_picks_max_in_range() {
        let range = r("[2.9.1,2.11)");
        let available = [
            "2.8.0", "2.9.0", "2.9.1", "2.10", "2.10.1", "2.11", "2.11.0",
        ]
        .map(String::from);
        assert_eq!(
            intersect_highest_satisfying(&[range], &available),
            Some("2.10.1".to_string())
        );
    }

    #[test]
    fn highest_satisfying_intersects_multiple_ranges() {
        let ranges = [r("[1.0,3.0)"), r("[2.0,)")];
        let available = ["1.5", "2.0", "2.5", "3.0", "3.5"].map(String::from);
        assert_eq!(
            intersect_highest_satisfying(&ranges, &available),
            Some("2.5".to_string())
        );
    }

    #[test]
    fn highest_satisfying_is_none_when_nothing_qualifies() {
        let range = r("[5.0,6.0)");
        let available = ["1.0", "2.0", "7.0"].map(String::from);
        assert_eq!(intersect_highest_satisfying(&[range], &available), None);
    }
}
