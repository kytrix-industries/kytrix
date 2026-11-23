#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptSpan {
    Control(String),
    Text(String),
}
