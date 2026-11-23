use std::fmt;

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum LanguageCode {
    Af,
    Sq,
    Am,
    Ar,
    Hy,
    As,
    Az,
    Eu,
    Bn,
    Bg,
    My,
    Ca,
    Chr,
    ZhHk,
    ZhCn,
    ZhTw,
    Hr,
    Cs,
    Da,
    Nl,
    EnGb,
    #[default]
    En,
    Et,
    Fil,
    Fi,
    Fr,
    FrCa,
    Gl,
    Ka,
    De,
    El,
    Gu,
    Iw,
    Hi,
    Hu,
    Is,
    In,
    Ga,
    It,
    Ja,
    Kn,
    Kk,
    Km,
    Ko,
    Lo,
    Lv,
    Lt,
    Mk,
    Ms,
    Ml,
    Mr,
    Mn,
    Ne,
    No,
    Or,
    Fa,
    Pl,
    PtBr,
    PtPt,
    Pa,
    Ro,
    Ru,
    Sr,
    Si,
    Sk,
    Sl,
    Es,
    Es419,
    Sw,
    Sv,
    Ta,
    Te,
    Th,
    Tr,
    Uk,
    Ur,
    Uz,
    Vi,
    Cy,
    Zu,
}

macro_rules! language_codes {
    ($($variant:ident => $code:literal, $name:literal;)*) => {
        pub(crate) const CODES: &[(&str, &str)] = &[$(($code, $name)),*];
        impl LanguageCode {
            pub const fn as_str(self) -> &'static str {
                const CODES: [&str; language_codes!(@count $($variant)*)] = [$($code),*];
                CODES[self as usize]
            }

            pub const fn name(self) -> &'static str {
                const NAMES: [&str; language_codes!(@count $($variant)*)] = [$($name),*];
                NAMES[self as usize]
            }

            pub fn parse(code: &str) -> Option<Self> {
                CODES
                    .iter()
                    .position(|(candidate, _)| *candidate == code)
                    .map(Self::from_index)
            }

            pub fn all() -> impl ExactSizeIterator<Item = Self> {
                const VARIANTS: [LanguageCode; language_codes!(@count $($variant)*)] =
                    [$(LanguageCode::$variant),*];

                VARIANTS.into_iter()
            }

            #[inline]
            const fn from_index(index: usize) -> Self {
                const VARIANTS: [LanguageCode; language_codes!(@count $($variant)*)] =
                    [$(LanguageCode::$variant),*];

                VARIANTS[index]
            }
        }

        #[cfg(test)]
        impl LanguageCode {
            pub(super) fn all_reachable() -> Vec<Self> {
                vec![$(LanguageCode::$variant),*]
            }
        }
    };
    (@count $($variant:ident)*) => {
        <[()]>::len(&[$(language_codes!(@unit $variant)),*])
    };
    (@unit $variant:ident) => {
        ()
    };
}

language_codes! {
    Af => "af", "Afrikaans";
    Sq => "sq", "Albanian";
    Am => "am", "Amharic";
    Ar => "ar", "Arabic";
    Hy => "hy", "Armenian";
    As => "as", "Assamese";
    Az => "az", "Azerbaijani";
    Eu => "eu", "Basque";
    Bn => "bn", "Bengali";
    Bg => "bg", "Bulgarian";
    My => "my", "Burmese";
    Ca => "ca", "Catalan";
    Chr => "chr", "Cherokee";
    ZhHk => "zh-HK", "Chinese (Hong Kong)";
    ZhCn => "zh-CN", "Chinese (Simplified)";
    ZhTw => "zh-TW", "Chinese (Traditional)";
    Hr => "hr", "Croatian";
    Cs => "cs", "Czech";
    Da => "da", "Danish";
    Nl => "nl", "Dutch";
    EnGb => "en-GB", "English (UK)";
    En => "en", "English (US)";
    Et => "et", "Estonian";
    Fil => "fil", "Filipino";
    Fi => "fi", "Finnish";
    Fr => "fr", "French";
    FrCa => "fr-CA", "French (Canada)";
    Gl => "gl", "Galician";
    Ka => "ka", "Georgian";
    De => "de", "German";
    El => "el", "Greek";
    Gu => "gu", "Gujarati";
    Iw => "iw", "Hebrew";
    Hi => "hi", "Hindi";
    Hu => "hu", "Hungarian";
    Is => "is", "Icelandic";
    In => "in", "Indonesian";
    Ga => "ga", "Irish";
    It => "it", "Italian";
    Ja => "ja", "Japanese";
    Kn => "kn", "Kannada";
    Kk => "kk", "Kazakh";
    Km => "km", "Khmer";
    Ko => "ko", "Korean";
    Lo => "lo", "Lao";
    Lv => "lv", "Latvian";
    Lt => "lt", "Lithuanian";
    Mk => "mk", "Macedonian";
    Ms => "ms", "Malay";
    Ml => "ml", "Malayalam";
    Mr => "mr", "Marathi";
    Mn => "mn", "Mongolian";
    Ne => "ne", "Nepali";
    No => "no", "Norwegian";
    Or => "or", "Odia (Oriya)";
    Fa => "fa", "Persian";
    Pl => "pl", "Polish";
    PtBr => "pt-BR", "Portuguese (Brazil)";
    PtPt => "pt-PT", "Portuguese (Portugal)";
    Pa => "pa", "Punjabi";
    Ro => "ro", "Romanian";
    Ru => "ru", "Russian";
    Sr => "sr", "Serbian";
    Si => "si", "Sinhala";
    Sk => "sk", "Slovak";
    Sl => "sl", "Slovenian";
    Es => "es", "Spanish";
    Es419 => "es-419", "Spanish (Latin America)";
    Sw => "sw", "Swahili";
    Sv => "sv", "Swedish";
    Ta => "ta", "Tamil";
    Te => "te", "Telugu";
    Th => "th", "Thai";
    Tr => "tr", "Turkish";
    Uk => "uk", "Ukrainian";
    Ur => "ur", "Urdu";
    Uz => "uz", "Uzbek";
    Vi => "vi", "Vietnamese";
    Cy => "cy", "Welsh";
    Zu => "zu", "Zulu";
}

impl fmt::Display for LanguageCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
