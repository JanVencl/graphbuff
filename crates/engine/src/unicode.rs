//! Unicode normalization (UAX #15): NFC, NFD, NFKC and NFKD, for `normalize()` and
//! `IS NORMALIZED`. The tables are generated from the Unicode Character Database
//! (`unicode/tables.rs`).

mod tables;

use tables::*;

/// A Unicode normalization form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalForm {
    Nfc,
    Nfd,
    Nfkc,
    Nfkd,
}

impl NormalForm {
    /// The form named `name` (`NFC`, `NFD`, `NFKC` or `NFKD`, any case).
    pub fn parse(name: &str) -> Option<NormalForm> {
        Some(match name.to_ascii_uppercase().as_str() {
            "NFC" => NormalForm::Nfc,
            "NFD" => NormalForm::Nfd,
            "NFKC" => NormalForm::Nfkc,
            "NFKD" => NormalForm::Nfkd,
            _ => return None,
        })
    }

    fn compat(self) -> bool {
        matches!(self, NormalForm::Nfkc | NormalForm::Nfkd)
    }

    fn composed(self) -> bool {
        matches!(self, NormalForm::Nfc | NormalForm::Nfkc)
    }
}

/// `s` in normal form `form`.
pub fn normalize(s: &str, form: NormalForm) -> String {
    // Fast path: ASCII is in every normal form.
    if s.is_ascii() {
        return s.to_string();
    }
    let mut chars = decompose(s, form.compat());
    if form.composed() {
        compose(&mut chars);
    }
    chars.into_iter().collect()
}

/// Whether `s` is in normal form `form`.
pub fn is_normalized(s: &str, form: NormalForm) -> bool {
    s.is_ascii() || normalize(s, form) == s
}

// Hangul syllables are composed and decomposed algorithmically (Unicode §3.12).
const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

fn combining_class(c: char) -> u8 {
    if c < '\u{300}' {
        return 0;
    }
    match COMBINING_CLASS.binary_search_by(|&(lo, hi, _)| {
        if hi < c {
            std::cmp::Ordering::Less
        } else if lo > c {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(i) => COMBINING_CLASS[i].2,
        Err(_) => 0,
    }
}

fn lookup<'a>(table: &[(char, u16, u8)], chars: &'a [char], c: char) -> Option<&'a [char]> {
    let i = table.binary_search_by_key(&c, |e| e.0).ok()?;
    let (_, start, len) = table[i];
    Some(&chars[start as usize..start as usize + len as usize])
}

/// The full decomposition of `s` (compatibility if `compat`), canonically ordered.
fn decompose(s: &str, compat: bool) -> Vec<char> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let s_index = (c as u32).wrapping_sub(S_BASE);
        if s_index < S_COUNT {
            out.push(char::from_u32(L_BASE + s_index / N_COUNT).expect("Hangul jamo"));
            out.push(char::from_u32(V_BASE + (s_index % N_COUNT) / T_COUNT).expect("Hangul jamo"));
            let t = s_index % T_COUNT;
            if t != 0 {
                out.push(char::from_u32(T_BASE + t).expect("Hangul jamo"));
            }
            continue;
        }
        let d = if compat { lookup(COMPAT, COMPAT_CHARS, c) } else { None };
        match d.or_else(|| lookup(CANONICAL, CANONICAL_CHARS, c)) {
            Some(d) => out.extend_from_slice(d),
            None => out.push(c),
        }
    }
    reorder(&mut out);
    out
}

/// Canonical ordering: a stable sort of each run of non-starters by combining class.
fn reorder(chars: &mut [char]) {
    let mut i = 0;
    while i < chars.len() {
        if combining_class(chars[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && combining_class(chars[i]) != 0 {
            i += 1;
        }
        if i - start > 1 {
            chars[start..i].sort_by_key(|&c| combining_class(c));
        }
    }
}

/// The primary composite of `a` and `b`, if any.
fn compose_pair(a: char, b: char) -> Option<char> {
    let (a32, b32) = (a as u32, b as u32);
    // L + V -> LV syllable.
    if (L_BASE..L_BASE + L_COUNT).contains(&a32) && (V_BASE..V_BASE + V_COUNT).contains(&b32) {
        let s = S_BASE + ((a32 - L_BASE) * V_COUNT + (b32 - V_BASE)) * T_COUNT;
        return char::from_u32(s);
    }
    // LV + T -> LVT syllable.
    let s_index = a32.wrapping_sub(S_BASE);
    if s_index < S_COUNT && s_index % T_COUNT == 0 && (T_BASE + 1..T_BASE + T_COUNT).contains(&b32) {
        return char::from_u32(a32 + b32 - T_BASE);
    }
    COMPOSITION
        .binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b)))
        .ok()
        .map(|i| COMPOSITION[i].2)
}

/// Canonical composition of a decomposed, canonically ordered sequence.
fn compose(chars: &mut Vec<char>) {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut starter: Option<usize> = None;
    let mut last_class = 0u8;
    for &c in chars.iter() {
        let class = combining_class(c);
        if let Some(si) = starter {
            // `c` is not blocked from the starter: nothing in between, or everything in
            // between has a lower (non-zero) combining class.
            let adjacent = si + 1 == out.len();
            if (adjacent || (last_class != 0 && last_class < class))
                && let Some(composite) = compose_pair(out[si], c)
            {
                out[si] = composite;
                continue;
            }
        }
        if class == 0 {
            starter = Some(out.len());
        }
        last_class = class;
        out.push(c);
    }
    *chars = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert_eq!(normalize("\u{212B}", NormalForm::Nfc), "\u{C5}");
        assert_eq!(normalize("\u{C5}", NormalForm::Nfd), "A\u{30A}");
        assert_eq!(normalize("\u{FE64}", NormalForm::Nfkc), "<");
        assert_eq!(normalize("\u{FE64}", NormalForm::Nfc), "\u{FE64}");
        assert_eq!(normalize("\u{FB01}", NormalForm::Nfkd), "fi");
        assert_eq!(normalize("\u{D55C}", NormalForm::Nfd), "\u{1112}\u{1161}\u{11AB}");
        assert_eq!(normalize("\u{1112}\u{1161}\u{11AB}", NormalForm::Nfc), "\u{D55C}");
        // Canonical ordering: dot below (220) before dot above (230).
        assert_eq!(normalize("q\u{307}\u{323}", NormalForm::Nfd), "q\u{323}\u{307}");
        assert_eq!(normalize("s\u{323}\u{307}", NormalForm::Nfc), "\u{1E69}");
        assert!(is_normalized("\u{C5}", NormalForm::Nfc));
        assert!(!is_normalized("\u{C5}", NormalForm::Nfd));
        assert_eq!(UNICODE_VERSION, "17.0.0");
    }

    /// Checks every line of the Unicode conformance test file, if `NORMALIZATION_TEST`
    /// names it (https://www.unicode.org/Public/<version>/ucd/NormalizationTest.txt).
    #[test]
    fn conformance() {
        let Ok(path) = std::env::var("NORMALIZATION_TEST") else {
            return;
        };
        let text = std::fs::read_to_string(path).expect("NormalizationTest.txt");
        let mut listed = std::collections::HashSet::new();
        let mut part1 = false;
        let mut checked = 0;
        for line in text.lines() {
            if line.starts_with("@Part") {
                part1 = line.starts_with("@Part1");
                continue;
            }
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let cols: Vec<String> = line
                .split(';')
                .take(5)
                .map(|f| {
                    f.split_whitespace()
                        .map(|h| char::from_u32(u32::from_str_radix(h, 16).unwrap()).unwrap())
                        .collect()
                })
                .collect();
            let [c1, c2, c3, c4, c5] = &cols[..] else { panic!("{line}") };
            for x in [c1, c2, c3] {
                assert_eq!(&normalize(x, NormalForm::Nfc), c2, "NFC {line}");
                assert_eq!(&normalize(x, NormalForm::Nfd), c3, "NFD {line}");
            }
            for x in [c4, c5] {
                assert_eq!(&normalize(x, NormalForm::Nfc), c4, "NFC {line}");
                assert_eq!(&normalize(x, NormalForm::Nfd), c5, "NFD {line}");
            }
            for x in [c1, c2, c3, c4, c5] {
                assert_eq!(&normalize(x, NormalForm::Nfkc), c4, "NFKC {line}");
                assert_eq!(&normalize(x, NormalForm::Nfkd), c5, "NFKD {line}");
            }
            if part1 {
                listed.insert(c1.chars().next().unwrap());
            }
            checked += 1;
        }
        // Every char not in part 1 is unchanged by every form.
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32).filter(|c| !listed.contains(c)) {
            let s = c.to_string();
            for form in [NormalForm::Nfc, NormalForm::Nfd, NormalForm::Nfkc, NormalForm::Nfkd] {
                assert_eq!(normalize(&s, form), s, "{form:?} U+{:04X}", c as u32);
            }
        }
        assert!(checked > 10_000);
    }
}
