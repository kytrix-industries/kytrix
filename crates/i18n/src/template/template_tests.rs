use super::{error::TemplateError, interpolate::Template, parts::TemplatePart};

fn render(template: &Template, arguments: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut output = String::new();
    template.write_with(arguments, &mut output);
    output
}

#[test]
fn text_without_placeholders_is_static() {
    let template = Template::parse("Market").expect("valid template");
    assert!(!template.is_interpolated());
    assert_eq!(template.text(), Some("Market"));
    assert_eq!(render(&template, &[]), "Market");
}

#[test]
fn empty_text_is_static() {
    let template = Template::parse("").expect("valid template");
    assert!(!template.is_interpolated());
    assert_eq!(template.text(), Some(""));
    assert_eq!(render(&template, &[]), "");
}

#[test]
fn single_placeholder_splits_into_parts() {
    let template = Template::parse("Hello, %{name}!").expect("valid template");
    let Template::Interpolated { parts } = &template else {
        panic!("expected an interpolated template");
    };

    assert_eq!(template.text(), None);
    assert_eq!(
        parts.as_ref(),
        [
            TemplatePart::Text("Hello, ".into()),
            TemplatePart::Placeholder("name".into()),
            TemplatePart::Text("!".into()),
        ]
    );
}

#[test]
fn placeholder_at_start_skips_empty_literal() {
    let template = Template::parse("%{name} arrived").expect("valid template");
    let Template::Interpolated { parts } = &template else {
        panic!("expected an interpolated template");
    };

    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], TemplatePart::Placeholder("name".into()));
    assert_eq!(parts[1], TemplatePart::Text(" arrived".into()));
}

#[test]
fn placeholder_at_end_skips_empty_literal() {
    let template = Template::parse("Hello %{name}").expect("valid template");
    let Template::Interpolated { parts } = &template else {
        panic!("expected an interpolated template");
    };

    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0], TemplatePart::Text("Hello ".into()));
    assert_eq!(parts[1], TemplatePart::Placeholder("name".into()));
}

#[test]
fn adjacent_placeholders_are_preserved() {
    let template = Template::parse("%{first}%{last}").expect("valid template");
    assert_eq!(
        template.placeholder_names().collect::<Vec<_>>(),
        ["first", "last"]
    );
    assert_eq!(render(&template, &[("first", &"A"), ("last", &"B")]), "AB");
}

#[test]
fn interpolates_every_supported_value_kind() {
    let template =
        Template::parse("Hello, %{name}! You have %{count} new %{kind}.").expect("valid template");
    let count = 3;
    let kind = String::from("messages");

    assert_eq!(
        render(
            &template,
            &[("name", &"Ada"), ("count", &count), ("kind", &kind)]
        ),
        "Hello, Ada! You have 3 new messages."
    );
}

#[test]
fn missing_arguments_render_as_empty_text() {
    let template = Template::parse("[%{missing}]").expect("valid template");
    assert_eq!(render(&template, &[("other", &"x")]), "[]");
}

#[test]
fn repeated_placeholders_share_one_argument() {
    let template = Template::parse("%{a} and %{a}").expect("valid template");
    assert_eq!(render(&template, &[("a", &"x")]), "x and x");
}

#[test]
fn placeholders_keep_non_ascii_text_intact() {
    let template = Template::parse("你好，%{name}！").expect("valid template");
    assert_eq!(render(&template, &[("name", &"世界")]), "你好，世界！");
}

#[test]
fn dangling_percent_is_literal_text() {
    let template = Template::parse("100% done").expect("valid template");
    assert!(!template.is_interpolated());
    assert_eq!(render(&template, &[]), "100% done");
}

#[test]
fn doubled_percent_is_literal_text() {
    let template = Template::parse("100%%{name}").expect("valid template");
    assert_eq!(render(&template, &[("name", &"!")]), "100%!");
}

#[test]
fn placeholder_names_include_repeats_in_order() {
    let template = Template::parse("%{b}-%{a}-%{b}").expect("valid template");
    assert_eq!(
        template.placeholder_names().collect::<Vec<_>>(),
        ["b", "a", "b"]
    );
}

#[test]
fn placeholders_may_contain_dots() {
    let template = Template::parse("%{user.name}").expect("valid template");
    assert_eq!(
        template.placeholder_names().collect::<Vec<_>>(),
        ["user.name"]
    );
    assert_eq!(render(&template, &[("user.name", &"Ada")]), "Ada");
}

#[test]
fn unclosed_placeholder_reports_its_offset() {
    assert_eq!(
        Template::parse("Hello, %{name").unwrap_err(),
        TemplateError::UnclosedPlaceholder { at: 7 }
    );
}

#[test]
fn unclosed_placeholder_reports_offset_after_non_ascii_text() {
    assert_eq!(
        Template::parse("你好 %{name").unwrap_err(),
        TemplateError::UnclosedPlaceholder { at: 7 }
    );
}

#[test]
fn empty_placeholder_is_rejected() {
    assert_eq!(
        Template::parse("Hello, %{}!").unwrap_err(),
        TemplateError::EmptyPlaceholder { at: 7 }
    );
}
