//! Lenient version and version-range model.
//!
//! Minecraft, loader and mod versions are not semver: they mix dotted numbers,
//! qualifiers (`1.21-pre1`, `0.15.0-beta.1`) and plain strings. This model splits
//! a version into numeric/text segments and orders them with the semver
//! "prerelease sorts before release" rule, so `1.20.1 < 1.21-pre1 < 1.21`.

use std::cmp::Ordering;
use std::fmt;

use super::error::ContentError;

#[derive(Debug, Clone)]
pub struct Version {
    raw: String,
    parts: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Num(u64),
    Text(String),
}

impl Version {
    pub fn parse(value: &str) -> Result<Self, ContentError> {
        let raw = value.trim();
        if raw.is_empty() || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err(ContentError::InvalidVersion {
                value: value.to_string(),
            });
        }
        let parts = tokenize(raw);
        if parts.is_empty() {
            return Err(ContentError::InvalidVersion {
                value: value.to_string(),
            });
        }
        Ok(Self {
            raw: raw.to_string(),
            parts,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

fn tokenize(raw: &str) -> Vec<Segment> {
    let mut parts = Vec::new();
    let mut buffer = String::new();
    let mut buffer_is_digit = false;

    for ch in raw.chars() {
        if !ch.is_alphanumeric() {
            if !buffer.is_empty() {
                push_segment(&mut parts, &mut buffer, buffer_is_digit);
            }
            continue;
        }
        let is_digit = ch.is_ascii_digit();
        if !buffer.is_empty() && is_digit != buffer_is_digit {
            push_segment(&mut parts, &mut buffer, buffer_is_digit);
        }
        buffer_is_digit = is_digit;
        buffer.push(ch.to_ascii_lowercase());
    }
    if !buffer.is_empty() {
        push_segment(&mut parts, &mut buffer, buffer_is_digit);
    }
    parts
}

fn push_segment(parts: &mut Vec<Segment>, buffer: &mut String, is_digit: bool) {
    let taken = std::mem::take(buffer);
    if is_digit {
        match taken.parse::<u64>() {
            Ok(value) => parts.push(Segment::Num(value)),
            Err(_) => parts.push(Segment::Text(taken)),
        }
    } else {
        parts.push(Segment::Text(taken));
    }
}

fn compare_segments(left: &Segment, right: &Segment) -> Ordering {
    match (left, right) {
        (Segment::Num(a), Segment::Num(b)) => a.cmp(b),
        (Segment::Text(a), Segment::Text(b)) => a.cmp(b),
        // A qualifier sorts before the release it qualifies.
        (Segment::Num(_), Segment::Text(_)) => Ordering::Greater,
        (Segment::Text(_), Segment::Num(_)) => Ordering::Less,
    }
}

/// Split into the leading numeric release part (trailing zeros dropped) and the
/// qualifier remainder, which is where the "prerelease sorts first" rule applies.
fn split_release(parts: &[Segment]) -> (Vec<u64>, &[Segment]) {
    let mut index = 0;
    let mut numbers = Vec::new();
    while let Some(Segment::Num(value)) = parts.get(index) {
        numbers.push(*value);
        index += 1;
    }
    while numbers.last() == Some(&0) {
        numbers.pop();
    }
    (numbers, &parts[index..])
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let (left_numbers, left_rest) = split_release(&self.parts);
        let (right_numbers, right_rest) = split_release(&other.parts);

        let numbers_len = left_numbers.len().max(right_numbers.len());
        for index in 0..numbers_len {
            let left = left_numbers.get(index).copied().unwrap_or(0);
            let right = right_numbers.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                other => return other,
            }
        }

        // A release sorts after its own qualifiers: 1.0-beta < 1.0.
        return match (left_rest.is_empty(), right_rest.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                let len = left_rest.len().min(right_rest.len());
                for index in 0..len {
                    match compare_segments(&left_rest[index], &right_rest[index]) {
                        Ordering::Equal => {}
                        other => return other,
                    }
                }
                left_rest.len().cmp(&right_rest.len())
            }
        };
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Bound {
    version: Version,
    inclusive: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Clause {
    lower: Option<Bound>,
    upper: Option<Bound>,
}

impl Clause {
    fn unbounded() -> Self {
        Self::default()
    }

    fn add_lower(&mut self, version: Version, inclusive: bool) {
        let replace = match &self.lower {
            None => true,
            Some(existing) => match version.cmp(&existing.version) {
                Ordering::Greater => true,
                Ordering::Equal => inclusive && !existing.inclusive,
                Ordering::Less => false,
            },
        };
        if replace {
            self.lower = Some(Bound { version, inclusive });
        }
    }

    fn add_upper(&mut self, version: Version, inclusive: bool) {
        let replace = match &self.upper {
            None => true,
            Some(existing) => match version.cmp(&existing.version) {
                Ordering::Less => true,
                Ordering::Equal => inclusive && !existing.inclusive,
                Ordering::Greater => false,
            },
        };
        if replace {
            self.upper = Some(Bound { version, inclusive });
        }
    }

    fn contains(&self, version: &Version) -> bool {
        let above = match &self.lower {
            None => true,
            Some(bound) => {
                let ordering = version.cmp(&bound.version);
                if bound.inclusive {
                    ordering != Ordering::Less
                } else {
                    ordering == Ordering::Greater
                }
            }
        };
        if !above {
            return false;
        }
        match &self.upper {
            None => true,
            Some(bound) => {
                let ordering = version.cmp(&bound.version);
                if bound.inclusive {
                    ordering != Ordering::Greater
                } else {
                    ordering == Ordering::Less
                }
            }
        }
    }
}

/// A disjunction of clauses (separated by `||`). Each clause is a conjunction of
/// comparators, so `>=1.20 <1.21` is one clause with two bounds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRange {
    raw: String,
    clauses: Vec<Clause>,
}

impl VersionRange {
    pub fn any() -> Self {
        Self {
            raw: "*".to_string(),
            clauses: vec![Clause::unbounded()],
        }
    }

    pub fn exact(version: Version) -> Self {
        let raw = version.as_str().to_string();
        let mut clause = Clause::unbounded();
        clause.add_lower(version.clone(), true);
        clause.add_upper(version, true);
        Self {
            raw,
            clauses: vec![clause],
        }
    }

    pub fn parse(value: &str) -> Result<Self, ContentError> {
        let raw = value.trim();
        if raw.is_empty() || raw == "*" || raw.eq_ignore_ascii_case("any") {
            return Ok(Self::any());
        }
        let mut clauses = Vec::new();
        for part in raw.split("||") {
            let part = part.trim();
            if part.is_empty() {
                return Err(ContentError::InvalidVersionRange {
                    value: value.to_string(),
                });
            }
            clauses.push(parse_clause(part, value)?);
        }
        if clauses.is_empty() {
            return Err(ContentError::InvalidVersionRange {
                value: value.to_string(),
            });
        }
        Ok(Self {
            raw: raw.to_string(),
            clauses,
        })
    }

    pub fn satisfies(&self, version: &Version) -> bool {
        self.clauses.iter().any(|clause| clause.contains(version))
    }

    pub fn is_any(&self) -> bool {
        self.clauses
            .iter()
            .any(|clause| clause.lower.is_none() && clause.upper.is_none())
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

fn parse_clause(part: &str, original: &str) -> Result<Clause, ContentError> {
    let mut clause = Clause::unbounded();
    let mut consumed = false;

    if part.starts_with('[') || part.starts_with('(') {
        let lower_inclusive = part.starts_with('[');
        let upper_inclusive = match part.chars().last() {
            Some(']') => true,
            Some(')') => false,
            _ => {
                return Err(ContentError::InvalidVersionRange {
                    value: original.to_string(),
                });
            }
        };
        if part.len() < 3 {
            return Err(ContentError::InvalidVersionRange {
                value: original.to_string(),
            });
        }
        let inner = &part[1..part.len() - 1];
        let pieces: Vec<&str> = inner.split(',').collect();
        if pieces.len() != 2 {
            return Err(ContentError::InvalidVersionRange {
                value: original.to_string(),
            });
        }
        let lower = pieces[0].trim();
        let upper = pieces[1].trim();
        if !lower.is_empty() {
            clause.add_lower(Version::parse(lower)?, lower_inclusive);
            consumed = true;
        }
        if !upper.is_empty() {
            clause.add_upper(Version::parse(upper)?, upper_inclusive);
            consumed = true;
        }
    } else {
        for token in part.split(|ch: char| ch.is_whitespace() || ch == ',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let (operator, rest) = split_operator(token);
            if operator.is_empty()
                && !rest
                    .chars()
                    .next()
                    .map(|ch| ch.is_alphanumeric())
                    .unwrap_or(false)
            {
                // `~1.2`, `^1.2` and friends are not supported; refuse instead of
                // silently reading them as an exact version.
                return Err(ContentError::InvalidVersionRange {
                    value: original.to_string(),
                });
            }
            let version = Version::parse(rest)?;
            match operator {
                ">=" => clause.add_lower(version, true),
                ">" => clause.add_lower(version, false),
                "<=" => clause.add_upper(version, true),
                "<" => clause.add_upper(version, false),
                "=" | "==" | "" => {
                    clause.add_lower(version.clone(), true);
                    clause.add_upper(version, true);
                }
                _ => {
                    return Err(ContentError::InvalidVersionRange {
                        value: original.to_string(),
                    });
                }
            }
            consumed = true;
        }
    }

    if !consumed {
        return Err(ContentError::InvalidVersionRange {
            value: original.to_string(),
        });
    }
    Ok(clause)
}

fn split_operator(token: &str) -> (&'static str, &str) {
    for operator in [">=", "<=", "==", ">", "<", "="] {
        if let Some(rest) = token.strip_prefix(operator) {
            return (operator_static(operator), rest.trim());
        }
    }
    ("", token)
}

fn operator_static(operator: &str) -> &'static str {
    match operator {
        ">=" => ">=",
        "<=" => "<=",
        "==" => "==",
        ">" => ">",
        "<" => "<",
        "=" => "=",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(value: &str) -> Version {
        Version::parse(value).expect("valid version")
    }

    fn range(value: &str) -> VersionRange {
        VersionRange::parse(value).expect("valid range")
    }

    #[test]
    fn orders_release_and_qualifier_segments() {
        assert!(version("1.20.1") > version("1.20"));
        assert!(version("1.21-pre1") < version("1.21"));
        assert!(version("1.21-pre2") > version("1.21-pre1"));
        assert!(version("47.2.0") > version("47.1.9"));
        assert_eq!(version("1.0"), version("1.0.0"));
        assert!(version("0.15.0-beta.1") < version("0.15.0"));
    }

    #[test]
    fn parses_comparator_and_maven_ranges() {
        assert!(range(">=1.20 <1.21").satisfies(&version("1.20.4")));
        assert!(!range(">=1.20 <1.21").satisfies(&version("1.21")));
        assert!(range("[1.20,1.21)").satisfies(&version("1.20")));
        assert!(!range("[1.20,1.21)").satisfies(&version("1.21")));
        assert!(range("(1.20,1.21]").satisfies(&version("1.21")));
        assert!(!range("(1.20,1.21]").satisfies(&version("1.20")));
        assert!(range("1.20.1").satisfies(&version("1.20.1")));
        assert!(!range("1.20.1").satisfies(&version("1.20.2")));
        assert!(range("*").satisfies(&version("0.0.1")));
        assert!(range(">=1.19 || 1.18.2").satisfies(&version("1.18.2")));
        assert!(!range(">=1.19 || 1.18.2").satisfies(&version("1.18.1")));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(Version::parse("").is_err());
        assert!(Version::parse("1 .20").is_err());
        assert!(VersionRange::parse(">=").is_err());
        assert!(VersionRange::parse("[1.0,").is_err());
        assert!(VersionRange::parse(">=1.0 ||").is_err());
        assert!(VersionRange::parse("~1.2").is_err());
    }
}
