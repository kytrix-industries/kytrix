use std::fmt;

/// A language supported by the Google Workspace Admin SDK.
///
/// Source: <https://developers.google.com/workspace/admin/directory/v1/languages>.
///
/// The [`LanguageCode::as_str`] values are the exact strings Google Workspace
/// expects in `languageCode` fields and `Accept-Language` headers, including the
/// legacy `iw` (Hebrew) and `in` (Indonesian) spellings. They also name the
/// locale files loaded by [`I18n::builder`](crate::I18n::builder), e.g. `zh-CN.yaml`.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum LanguageCode {
    /// Afrikaans.
    Af,
    /// Albanian.
    Sq,
    /// Amharic.
    Am,
    /// Arabic.
    Ar,
    /// Armenian.
    Hy,
    /// Assamese.
    As,
    /// Azerbaijani.
    Az,
    /// Basque.
    Eu,
    /// Bengali.
    Bn,
    /// Bulgarian.
    Bg,
    /// Burmese.
    My,
    /// Catalan.
    Ca,
    /// Cherokee.
    Chr,
    /// Chinese (Hong Kong).
    ZhHk,
    /// Chinese (Simplified).
    ZhCn,
    /// Chinese (Traditional).
    ZhTw,
    /// Croatian.
    Hr,
    /// Czech.
    Cs,
    /// Danish.
    Da,
    /// Dutch.
    Nl,
    /// English (United Kingdom).
    EnGb,
    /// English (United States).
    #[default]
    En,
    /// Estonian.
    Et,
    /// Filipino.
    Fil,
    /// Finnish.
    Fi,
    /// French.
    Fr,
    /// French (Canada).
    FrCa,
    /// Galician.
    Gl,
    /// Georgian.
    Ka,
    /// German.
    De,
    /// Greek.
    El,
    /// Gujarati.
    Gu,
    /// Hebrew.
    Iw,
    /// Hindi.
    Hi,
    /// Hungarian.
    Hu,
    /// Icelandic.
    Is,
    /// Indonesian.
    In,
    /// Irish.
    Ga,
    /// Italian.
    It,
    /// Japanese.
    Ja,
    /// Kannada.
    Kn,
    /// Kazakh.
    Kk,
    /// Khmer.
    Km,
    /// Korean.
    Ko,
    /// Lao.
    Lo,
    /// Latvian.
    Lv,
    /// Lithuanian.
    Lt,
    /// Macedonian.
    Mk,
    /// Malay.
    Ms,
    /// Malayalam.
    Ml,
    /// Marathi.
    Mr,
    /// Mongolian.
    Mn,
    /// Nepali.
    Ne,
    /// Norwegian.
    No,
    /// Odia (Oriya).
    Or,
    /// Persian.
    Fa,
    /// Polish.
    Pl,
    /// Portuguese (Brazil).
    PtBr,
    /// Portuguese (Portugal).
    PtPt,
    /// Punjabi.
    Pa,
    /// Romanian.
    Ro,
    /// Russian.
    Ru,
    /// Serbian.
    Sr,
    /// Sinhala.
    Si,
    /// Slovak.
    Sk,
    /// Slovenian.
    Sl,
    /// Spanish.
    Es,
    /// Spanish (Latin America).
    Es419,
    /// Swahili.
    Sw,
    /// Swedish.
    Sv,
    /// Tamil.
    Ta,
    /// Telugu.
    Te,
    /// Thai.
    Th,
    /// Turkish.
    Tr,
    /// Ukrainian.
    Uk,
    /// Urdu.
    Ur,
    /// Uzbek.
    Uz,
    /// Vietnamese.
    Vi,
    /// Welsh.
    Cy,
    /// Zulu.
    Zu,
}

/// The language the interface is rendered in is a global, so switching it is
/// observable: a component that holds text read in the language it left can
/// follow a change instead of comparing against a copy of its own.
impl gpui_kit::Global for LanguageCode {}

/// Declares the code/name table and the matching variant list from one source of
/// truth, so `LanguageCode::as_str`, `LanguageCode::name` and `LanguageCode::all`
/// cannot drift apart.
macro_rules! language_codes {
    ($($variant:ident => $code:literal, $name:literal;)*) => {
        /// Every supported language code, as `(code, English name)` pairs.
        ///
        /// The order is the one the
        /// [Google Workspace language list](https://developers.google.com/workspace/admin/directory/v1/languages)
        /// uses, which is a sensible order for a language picker.
        ///
        /// [`LanguageCode::all`] walks the same list, so it is the only accessor
        /// consumers need.
        pub(crate) const CODES: &[(&str, &str)] = &[$(($code, $name)),*];
        impl LanguageCode {
            /// Returns the Google Workspace language code, e.g. `"zh-CN"`.
            pub const fn as_str(self) -> &'static str {
                const CODES: [&str; language_codes!(@count $($variant)*)] = [$($code),*];
                CODES[self as usize]
            }

            /// Returns the English name of the language, e.g. `"Chinese (Simplified)"`.
            pub const fn name(self) -> &'static str {
                const NAMES: [&str; language_codes!(@count $($variant)*)] = [$($name),*];
                NAMES[self as usize]
            }

            /// Parses a Google Workspace language code such as `"en"`, `"zh-CN"` or
            /// `"pt-BR"`.
            ///
            /// Returns `None` for anything that is not in the language list. The
            /// list holds fewer than a hundred short codes and is only searched
            /// while loading locale files, so a scan is cheaper than keeping a
            /// second, code-sorted copy of the table in the binary.
            pub fn parse(code: &str) -> Option<Self> {
                CODES
                    .iter()
                    .position(|(candidate, _)| *candidate == code)
                    .map(Self::from_index)
            }

            /// Returns every supported language code, in the order of the Google
            /// Workspace language list.
            pub fn all() -> impl ExactSizeIterator<Item = Self> {
                const VARIANTS: [LanguageCode; language_codes!(@count $($variant)*)] =
                    [$(LanguageCode::$variant),*];

                VARIANTS.into_iter()
            }

            /// Converts the position of a table entry to its variant.
            #[inline]
            const fn from_index(index: usize) -> Self {
                const VARIANTS: [LanguageCode; language_codes!(@count $($variant)*)] =
                    [$(LanguageCode::$variant),*];

                VARIANTS[index]
            }
        }

        #[cfg(test)]
        impl LanguageCode {
            /// Returns every variant, including any that
            /// [`LanguageCode::all`] might have missed.
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
