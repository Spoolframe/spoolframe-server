//! Language codes as files write them: ISO 639-2 in streams (`eng`, and
//! both `ger` and `deu`), often ISO 639-1 in subtitle file names (`en`).
//! [`same_language`] treats all spellings of one language as equal.

/// Two- and three-letter codes of one language: 639-1, 639-2/B, 639-2/T.
/// The languages files commonly carry; a code not here only equals itself.
const ALIASES: &[&[&str]] = &[
    &["ar", "ara"],
    &["bg", "bul"],
    &["ca", "cat"],
    &["cs", "cze", "ces"],
    &["da", "dan"],
    &["de", "ger", "deu"],
    &["el", "gre", "ell"],
    &["en", "eng"],
    &["es", "spa"],
    &["et", "est"],
    &["fa", "per", "fas"],
    &["fi", "fin"],
    &["fr", "fre", "fra"],
    &["he", "heb"],
    &["hi", "hin"],
    &["hr", "hrv"],
    &["hu", "hun"],
    &["id", "ind"],
    &["is", "ice", "isl"],
    &["it", "ita"],
    &["ja", "jpn"],
    &["ko", "kor"],
    &["lt", "lit"],
    &["lv", "lav"],
    &["ms", "may", "msa"],
    &["nb", "nob"],
    &["nl", "dut", "nld"],
    &["no", "nor"],
    &["pl", "pol"],
    &["pt", "por"],
    &["ro", "rum", "ron"],
    &["ru", "rus"],
    &["sk", "slo", "slk"],
    &["sl", "slv"],
    &["sr", "srp"],
    &["sv", "swe"],
    &["th", "tha"],
    &["tr", "tur"],
    &["uk", "ukr"],
    &["vi", "vie"],
    &["zh", "chi", "zho"],
];

/// One spelling for every code of a language: its first in [`ALIASES`],
/// or the code itself, lowercased.
fn canonical(code: &str) -> String {
    let code = code.to_ascii_lowercase();
    ALIASES
        .iter()
        .find(|codes| codes.contains(&code.as_str()))
        .map_or(code, |codes| codes[0].to_owned())
}

/// Whether two language codes name the same language.
pub fn same_language(a: &str, b: &str) -> bool {
    canonical(a) == canonical(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_of_one_language_are_the_same() {
        assert!(same_language("en", "eng"));
        assert!(same_language("ger", "deu"));
        assert!(same_language("DE", "ger"));
        assert!(same_language("chi", "zho"));
    }

    #[test]
    fn different_languages_differ() {
        assert!(!same_language("eng", "spa"));
        assert!(!same_language("nob", "nor"));
    }

    #[test]
    fn an_unknown_code_equals_only_itself() {
        assert!(same_language("tlh", "TLH"));
        assert!(!same_language("tlh", "eng"));
    }
}
