#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TargetLanguage(usize);

const TABLE: [(&str, &str); 38] = [
    ("en", "English"),
    ("zh", "Chinese"),
    ("fr", "French"),
    ("pt", "Portuguese"),
    ("es", "Spanish"),
    ("ja", "Japanese"),
    ("tr", "Turkish"),
    ("ru", "Russian"),
    ("ar", "Arabic"),
    ("ko", "Korean"),
    ("th", "Thai"),
    ("it", "Italian"),
    ("de", "German"),
    ("vi", "Vietnamese"),
    ("ms", "Malay"),
    ("id", "Indonesian"),
    ("tl", "Filipino"),
    ("hi", "Hindi"),
    ("zh-Hant", "Traditional Chinese"),
    ("pl", "Polish"),
    ("cs", "Czech"),
    ("nl", "Dutch"),
    ("km", "Khmer"),
    ("my", "Burmese"),
    ("fa", "Persian"),
    ("gu", "Gujarati"),
    ("ur", "Urdu"),
    ("te", "Telugu"),
    ("mr", "Marathi"),
    ("he", "Hebrew"),
    ("bn", "Bengali"),
    ("ta", "Tamil"),
    ("uk", "Ukrainian"),
    ("bo", "Tibetan"),
    ("kk", "Kazakh"),
    ("mn", "Mongolian"),
    ("ug", "Uyghur"),
    ("yue", "Cantonese"),
];

static ALL: [TargetLanguage; 38] = [
    TargetLanguage(0),
    TargetLanguage(1),
    TargetLanguage(2),
    TargetLanguage(3),
    TargetLanguage(4),
    TargetLanguage(5),
    TargetLanguage(6),
    TargetLanguage(7),
    TargetLanguage(8),
    TargetLanguage(9),
    TargetLanguage(10),
    TargetLanguage(11),
    TargetLanguage(12),
    TargetLanguage(13),
    TargetLanguage(14),
    TargetLanguage(15),
    TargetLanguage(16),
    TargetLanguage(17),
    TargetLanguage(18),
    TargetLanguage(19),
    TargetLanguage(20),
    TargetLanguage(21),
    TargetLanguage(22),
    TargetLanguage(23),
    TargetLanguage(24),
    TargetLanguage(25),
    TargetLanguage(26),
    TargetLanguage(27),
    TargetLanguage(28),
    TargetLanguage(29),
    TargetLanguage(30),
    TargetLanguage(31),
    TargetLanguage(32),
    TargetLanguage(33),
    TargetLanguage(34),
    TargetLanguage(35),
    TargetLanguage(36),
    TargetLanguage(37),
];

impl TargetLanguage {
    pub fn parse(code: &str) -> Option<Self> {
        TABLE
            .iter()
            .position(|(candidate, _)| candidate.eq_ignore_ascii_case(code))
            .map(Self)
    }

    pub fn code(self) -> &'static str {
        TABLE[self.0].0
    }

    pub fn english_name(self) -> &'static str {
        TABLE[self.0].1
    }

    pub fn all() -> &'static [TargetLanguage] {
        &ALL
    }
}

#[cfg(test)]
mod tests {
    use super::TargetLanguage;

    const COUNT: usize = 38;

    #[test]
    fn every_table_entry_parses_to_itself() {
        for language in TargetLanguage::all() {
            assert_eq!(TargetLanguage::parse(language.code()), Some(*language));
        }
    }

    #[test]
    fn codes_are_matched_case_insensitively() {
        for language in TargetLanguage::all() {
            let uppercase = language.code().to_uppercase();

            assert_eq!(TargetLanguage::parse(&uppercase), Some(*language));
        }
    }

    #[test]
    fn english_names_aliases_and_unknown_codes_are_rejected() {
        for code in [
            "English",
            "Chinese",
            "Traditional Chinese",
            "Cantonese",
            "zh-CN",
            "zh-TW",
            "xx",
            "",
        ] {
            assert_eq!(
                TargetLanguage::parse(code),
                None,
                "`{code}` must be rejected"
            );
        }
    }

    #[test]
    fn the_table_holds_thirty_eight_unique_codes() {
        assert_eq!(TargetLanguage::all().len(), COUNT);

        let mut codes: Vec<&str> = TargetLanguage::all()
            .iter()
            .map(|language| language.code())
            .collect();
        codes.sort_unstable();
        codes.dedup();

        assert_eq!(codes.len(), COUNT);
    }

    const EXPECTED: [(&str, &str); 38] = [
        ("en", "English"),
        ("zh", "Chinese"),
        ("fr", "French"),
        ("pt", "Portuguese"),
        ("es", "Spanish"),
        ("ja", "Japanese"),
        ("tr", "Turkish"),
        ("ru", "Russian"),
        ("ar", "Arabic"),
        ("ko", "Korean"),
        ("th", "Thai"),
        ("it", "Italian"),
        ("de", "German"),
        ("vi", "Vietnamese"),
        ("ms", "Malay"),
        ("id", "Indonesian"),
        ("tl", "Filipino"),
        ("hi", "Hindi"),
        ("zh-Hant", "Traditional Chinese"),
        ("pl", "Polish"),
        ("cs", "Czech"),
        ("nl", "Dutch"),
        ("km", "Khmer"),
        ("my", "Burmese"),
        ("fa", "Persian"),
        ("gu", "Gujarati"),
        ("ur", "Urdu"),
        ("te", "Telugu"),
        ("mr", "Marathi"),
        ("he", "Hebrew"),
        ("bn", "Bengali"),
        ("ta", "Tamil"),
        ("uk", "Ukrainian"),
        ("bo", "Tibetan"),
        ("kk", "Kazakh"),
        ("mn", "Mongolian"),
        ("ug", "Uyghur"),
        ("yue", "Cantonese"),
    ];

    #[test]
    fn every_code_has_its_model_card_name() {
        let actual: Vec<(&str, &str)> = TargetLanguage::all()
            .iter()
            .map(|language| (language.code(), language.english_name()))
            .collect();

        assert_eq!(actual, EXPECTED.to_vec());
    }

    #[test]
    fn names_are_the_exact_model_card_spellings() {
        assert_eq!(
            TargetLanguage::parse("zh-Hant").map(TargetLanguage::english_name),
            Some("Traditional Chinese")
        );
        assert_eq!(
            TargetLanguage::parse("yue").map(TargetLanguage::english_name),
            Some("Cantonese")
        );
        assert_eq!(
            TargetLanguage::parse("tl").map(TargetLanguage::english_name),
            Some("Filipino")
        );
        assert_eq!(
            TargetLanguage::parse("en").map(TargetLanguage::english_name),
            Some("English")
        );
    }
}
