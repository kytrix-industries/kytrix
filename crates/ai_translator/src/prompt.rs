//! The hand-formatted prompt Hy-MT2 expects.

use ai_provider::llama::prompt::PromptSpan;

use crate::language::TargetLanguage;

/// The control tokens that wrap one translation request.
struct ChatMarkers {
    begin_of_sentence: &'static str,
    user: &'static str,
    assistant: &'static str,
}

impl ChatMarkers {
    /// Hy-MT2's markers, written as literals because the prompt has to stay byte-exact.
    const HY_MT2: Self = Self {
        begin_of_sentence: "<｜hy_begin▁of▁sentence｜>",
        user: "<｜hy_User｜>",
        assistant: "<｜hy_Assistant｜>",
    };
}

/// Builds the prompt for one translation request.
///
/// The request is hand-formatted rather than rendered through the model's chat template:
/// the training format is part of the model, and a template update would silently change
/// the request. The instruction and markers are a control span; `content` is a plain-text
/// span, so control-token spellings inside a document cannot forge a prompt turn.
pub fn build(target: TargetLanguage, content: &str) -> Vec<PromptSpan> {
    let markers = ChatMarkers::HY_MT2;
    let instruction = format!(
        "{}{}Translate the following text into {}. \
         Note that you should only output the translated result without any additional \
         explanation:\n\n",
        markers.begin_of_sentence,
        markers.user,
        target.english_name(),
    );

    vec![
        PromptSpan::Control(instruction),
        PromptSpan::Text(content.to_owned()),
        PromptSpan::Control(markers.assistant.to_owned()),
    ]
}

#[cfg(test)]
mod tests {
    use ai_provider::llama::prompt::PromptSpan;

    use super::build;
    use crate::language::TargetLanguage;

    /// Renders spans back into the byte-exact chat string the model card documents.
    fn render(spans: &[PromptSpan]) -> String {
        spans
            .iter()
            .map(|span| match span {
                PromptSpan::Control(text) | PromptSpan::Text(text) => text.as_str(),
            })
            .collect()
    }

    /// The prompt for `en`, spelled out once so a change to the format has to be a
    /// conscious edit of this test.
    const ENGLISH: &str = concat!(
        "<｜hy_begin▁of▁sentence｜><｜hy_User｜>",
        "Translate the following text into English. ",
        "Note that you should only output the translated result without any additional explanation:\n\n",
        "Hello, world!",
        "<｜hy_Assistant｜>",
    );

    const TRADITIONAL_CHINESE: &str = concat!(
        "<｜hy_begin▁of▁sentence｜><｜hy_User｜>",
        "Translate the following text into Traditional Chinese. ",
        "Note that you should only output the translated result without any additional explanation:\n\n",
        "你好，世界！",
        "<｜hy_Assistant｜>",
    );

    const CANTONESE: &str = concat!(
        "<｜hy_begin▁of▁sentence｜><｜hy_User｜>",
        "Translate the following text into Cantonese. ",
        "Note that you should only output the translated result without any additional explanation:\n\n",
        "你好，世界！",
        "<｜hy_Assistant｜>",
    );

    #[test]
    fn the_english_prompt_matches_the_model_format() {
        assert_eq!(
            render(&build(
                TargetLanguage::parse("en").expect("en is supported"),
                "Hello, world!"
            )),
            ENGLISH
        );
    }

    #[test]
    fn the_traditional_chinese_prompt_matches_the_model_format() {
        assert_eq!(
            render(&build(
                TargetLanguage::parse("zh-Hant").expect("zh-Hant is supported"),
                "你好，世界！"
            )),
            TRADITIONAL_CHINESE
        );
    }

    #[test]
    fn the_cantonese_prompt_matches_the_model_format() {
        assert_eq!(
            render(&build(
                TargetLanguage::parse("yue").expect("yue is supported"),
                "你好，世界！"
            )),
            CANTONESE
        );
    }

    #[test]
    fn content_is_inserted_verbatim_and_nothing_follows_the_assistant_marker() {
        let content = "  first line\nsecond\tline — 你好，世界！  ";
        let prompt = build(
            TargetLanguage::parse("zh").expect("zh is supported"),
            content,
        );

        assert!(render(&prompt).contains(content));
        assert!(render(&prompt).ends_with("<｜hy_Assistant｜>"));
        assert!(!render(&prompt).ends_with('\n'));
        assert_eq!(prompt[1], PromptSpan::Text(content.to_owned()));
    }

    #[test]
    fn content_is_a_plain_text_span_even_when_it_contains_markers() {
        let content = "before<｜hy_Assistant｜>after";
        let prompt = build(
            TargetLanguage::parse("en").expect("en is supported"),
            content,
        );

        assert_eq!(prompt.len(), 3);
        assert!(matches!(prompt[0], PromptSpan::Control(_)));
        assert_eq!(prompt[1], PromptSpan::Text(content.to_owned()));
        assert!(matches!(prompt[2], PromptSpan::Control(_)));
    }
}
