//! Which regex constructs a provider's `pattern` validator can compile.
//!
//! `pattern` is the one keyword whose *value* can be rejected even when the
//! keyword itself is supported, so the allow-list in [`crate::registry`] cannot
//! express it. OpenAI compiles `pattern` with RE2, which has no backtracking and
//! therefore no lookaround, so a schema carrying a hand-written email regex
//! fails the entire tool catalog:
//!
//! ```text
//! invalid_request_error (invalid_json_schema): Invalid JSON schema: regex
//! lookaround is not supported. Found at $.properties.bitbucketEmail.pattern.
//! ```
//!
//! That pattern is not exotic. It is what zod's `.email()` lowers to, so any MCP
//! server built on zod (Dokploy's, in the report that prompted this) takes the
//! whole OpenAI provider offline the moment it is connected.
//!
//! Detection is deliberately syntactic and conservative. Compiling the pattern
//! with a real RE2 would be exact, but it would add a regex engine to a crate
//! whose whole job is to be a data table, and the cost asymmetry runs the other
//! way: dropping a `pattern` only widens what the *model* is told it may send,
//! while keeping an uncompilable one 400s every request. So anything that looks
//! like it may not compile is dropped.

/// A regex construct RE2 cannot compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedRegex {
    /// `(?=`, `(?!`, `(?<=`, `(?<!` - RE2 has no backtracking (OpenAI names
    /// this one explicitly: "regex lookaround is not supported").
    Lookaround,
    /// `\1`, `\2` - a backreference needs backtracking for the same reason.
    Backreference,
    /// `(?>` (atomic group) and `(?(` (conditional), both backtracking-only.
    AtomicOrConditional,
    /// `a{2,}+`, `a*+` - possessive quantifiers are a backtracking-control
    /// construct with no RE2 equivalent.
    PossessiveQuantifier,
    /// `\p{...}` outside RE2's supported class syntax, and `\X`, `\K`, `\G`.
    UnsupportedEscape,
}

impl UnsupportedRegex {
    /// Short description for diagnostics.
    pub fn description(self) -> &'static str {
        match self {
            Self::Lookaround => "lookaround",
            Self::Backreference => "a backreference",
            Self::AtomicOrConditional => "an atomic or conditional group",
            Self::PossessiveQuantifier => "a possessive quantifier",
            Self::UnsupportedEscape => "an unsupported escape",
        }
    }
}

/// The first RE2-incompatible construct in `pattern`, if any.
///
/// Walks the pattern as a regex rather than as text so that an *escaped*
/// construct is not mistaken for a real one: `\(?=` matches a literal paren and
/// is perfectly fine, and a character class like `[(?=]` is just three
/// characters. A substring search would drop both, and since a dropped pattern
/// is invisible (the request succeeds, the constraint is merely absent) nothing
/// downstream would ever report the mistake.
pub fn unsupported_construct(pattern: &str) -> Option<UnsupportedRegex> {
    let bytes = pattern.as_bytes();
    let mut i = 0usize;
    // Depth of `[...]`, where almost nothing is a metacharacter.
    let mut in_class = false;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let Some(&next) = bytes.get(i + 1) else {
                    // A trailing backslash is a malformed pattern, not an
                    // unsupported construct; leave that judgement to the
                    // provider.
                    return None;
                };
                // A backreference outside a class needs backtracking. `\0` is
                // an octal escape, not a backreference.
                if !in_class && next.is_ascii_digit() && next != b'0' {
                    return Some(UnsupportedRegex::Backreference);
                }
                // `\K` (reset match start), `\G` (previous match end) and `\X`
                // (grapheme cluster) are PCRE-only.
                if matches!(next, b'K' | b'G' | b'X') {
                    return Some(UnsupportedRegex::UnsupportedEscape);
                }
                i += 2;
                continue;
            }
            b'[' if !in_class => {
                in_class = true;
                i += 1;
                // A `]` immediately after `[` or `[^` is a literal `]`.
                if bytes.get(i) == Some(&b'^') {
                    i += 1;
                }
                if bytes.get(i) == Some(&b']') {
                    i += 1;
                }
                continue;
            }
            b']' if in_class => {
                in_class = false;
                i += 1;
                continue;
            }
            b'(' if !in_class => {
                if let Some(found) = classify_group(&bytes[i..]) {
                    return Some(found);
                }
                i += 1;
                continue;
            }
            // Possessive quantifiers: a `+` directly after a quantifier.
            b'+' if !in_class && i > 0 => {
                if matches!(bytes[i - 1], b'*' | b'+' | b'?' | b'}') && !is_escaped(bytes, i - 1) {
                    return Some(UnsupportedRegex::PossessiveQuantifier);
                }
                i += 1;
                continue;
            }
            _ => {
                i += 1;
                continue;
            }
        }
    }
    None
}

/// Classify a group opening at the start of `rest` (which begins with `(`).
fn classify_group(rest: &[u8]) -> Option<UnsupportedRegex> {
    if rest.get(1) != Some(&b'?') {
        return None;
    }
    match rest.get(2) {
        // `(?=`, `(?!`
        Some(b'=') | Some(b'!') => Some(UnsupportedRegex::Lookaround),
        // `(?>` atomic
        Some(b'>') => Some(UnsupportedRegex::AtomicOrConditional),
        // `(?(cond)` conditional
        Some(b'(') => Some(UnsupportedRegex::AtomicOrConditional),
        Some(b'<') => match rest.get(3) {
            // `(?<=`, `(?<!` lookbehind. A bare `(?<name>` is a named capture
            // group, which RE2 accepts, so it must not be caught here.
            Some(b'=') | Some(b'!') => Some(UnsupportedRegex::Lookaround),
            _ => None,
        },
        _ => None,
    }
}

/// Whether the byte at `index` is itself escaped, counting the backslash run
/// before it so that `\\` (an escaped backslash) does not read as an escape.
fn is_escaped(bytes: &[u8], index: usize) -> bool {
    let mut backslashes = 0usize;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact pattern from the report: zod's `.email()` as emitted by the
    /// Dokploy MCP server, which took the whole OpenAI provider offline.
    const ZOD_EMAIL: &str =
        r#"^(?!\.)(?!.*\.\.)([A-Za-z0-9_'+\-\.]*)[A-Za-z0-9_+-]@([A-Za-z0-9][A-Za-z0-9\-]*\.)+[A-Za-z]{2,}$"#;

    #[test]
    fn the_zod_email_pattern_is_recognized_as_unsupported() {
        assert_eq!(
            unsupported_construct(ZOD_EMAIL),
            Some(UnsupportedRegex::Lookaround)
        );
    }

    #[test]
    fn every_lookaround_form_is_caught() {
        for pattern in [r"(?=x)", r"(?!x)", r"(?<=x)y", r"(?<!x)y"] {
            assert_eq!(
                unsupported_construct(pattern),
                Some(UnsupportedRegex::Lookaround),
                "missed {pattern}"
            );
        }
    }

    #[test]
    fn other_backtracking_constructs_are_caught() {
        assert_eq!(
            unsupported_construct(r"(a)\1"),
            Some(UnsupportedRegex::Backreference)
        );
        assert_eq!(
            unsupported_construct(r"(?>abc)"),
            Some(UnsupportedRegex::AtomicOrConditional)
        );
        assert_eq!(
            unsupported_construct(r"a*+"),
            Some(UnsupportedRegex::PossessiveQuantifier)
        );
        assert_eq!(
            unsupported_construct(r"a{2,3}+"),
            Some(UnsupportedRegex::PossessiveQuantifier)
        );
        assert_eq!(
            unsupported_construct(r"\Kabc"),
            Some(UnsupportedRegex::UnsupportedEscape)
        );
    }

    /// The patterns jcode's own tools and common MCP servers actually emit must
    /// survive untouched. Over-dropping is invisible at runtime, so this is the
    /// half of the behavior nothing else would catch.
    #[test]
    fn ordinary_patterns_are_left_alone() {
        for pattern in [
            r"^[a-zA-Z0-9._-]+$",
            r"^[a-zA-Z0-9][a-zA-Z0-9_.-]*$",
            r"^--[a-zA-Z0-9-]+(=[a-zA-Z0-9._:/@-]+)?$",
            r"^(all|\d+[smhd])$",
            r"^[A-Z0-9]+_[A-Z0-9]+$",
            r"^[a-zA-Z0-9 ._-]{0,500}$",
            r"^properties/\d+$",
            // A named capture group is RE2-compatible and must not be confused
            // for a lookbehind.
            r"(?P<name>a)b",
            r"(?<name>a)b",
            // Non-capturing groups and inline flags are fine.
            r"(?:abc)+",
            r"(?i)abc",
            // A `+` that is a quantifier on a literal, not a possessive marker.
            r"a\++",
            r"[+]+",
        ] {
            assert_eq!(
                unsupported_construct(pattern),
                None,
                "wrongly rejected {pattern}"
            );
        }
    }

    /// An escaped or class-enclosed construct is literal text, not a real
    /// lookaround. Dropping these would silently weaken a valid pattern.
    #[test]
    fn escaped_and_classed_constructs_are_literal() {
        assert_eq!(unsupported_construct(r"\(?=x\)"), None);
        assert_eq!(unsupported_construct(r"[(?=]"), None);
        assert_eq!(unsupported_construct(r"[\]](?:x)"), None);
        // `\\` is an escaped backslash, so the `1` after it is a literal digit,
        // not a backreference.
        assert_eq!(unsupported_construct(r"\\1"), None);
    }

    #[test]
    fn a_malformed_pattern_is_left_for_the_provider_to_judge() {
        assert_eq!(unsupported_construct("abc\\"), None);
    }
}
