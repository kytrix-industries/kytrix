//! Prompt pieces with distinct tokenization rules.

/// One piece of a prompt.
///
/// The distinction is a safety boundary, not a formatting one: [`Self::Control`]
/// text is parsed for special/control tokens (the template's turn markers),
/// while [`Self::Text`] is tokenized as plain text. Without the split, text like
/// `<｜hy_Assistant｜>` inside a document would become a real turn marker and
/// could forge a prompt turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptSpan {
    /// Template scaffolding whose special-token spellings must be parsed.
    Control(String),
    /// Untrusted user text, tokenized with special-token parsing disabled.
    Text(String),
}
