#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    Hlsl,
    Metal,
    Glsl,
}

impl Language {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Hlsl => "HLSL",
            Self::Metal => "MSL",
            Self::Glsl => "GLSL",
        }
    }
}

pub struct ShaderSource {
    pub language: Language,
    pub files: Vec<(String, String)>,
}

impl ShaderSource {
    #[must_use]
    pub fn new(language: Language, files: Vec<(String, String)>) -> Self {
        Self { language, files }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    Number(String),
    Punct(char),
}

#[derive(Clone, Debug)]
struct Located {
    token: Token,
    line: usize,
}

#[must_use]
pub fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        match line.find("//") {
            Some(index) => {
                out.push_str(&line[..index]);
                out.extend(
                    line[index..]
                        .chars()
                        .map(|ch| if ch == '\n' { '\n' } else { ' ' }),
                );
            }
            None => out.push_str(line),
        }
    }
    out
}

fn tokenize(src: &str) -> Vec<Located> {
    let mut tokens = Vec::new();
    let mut line = 1usize;
    let chars: Vec<char> = src.chars().collect();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\n' {
            line += 1;
            index += 1;
            continue;
        }
        if ch.is_whitespace() {
            index += 1;
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            tokens.push(Located {
                token: Token::Ident(
                    chars[start..index]
                        .iter()
                        .collect::<String>()
                        .to_ascii_lowercase(),
                ),
                line,
            });
            continue;
        }
        if ch.is_ascii_digit()
            || (ch == '.' && index + 1 < chars.len() && chars[index + 1].is_ascii_digit())
        {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric()
                    || chars[index] == '.'
                    || chars[index] == '+'
                    || chars[index] == '-')
            {
                if (chars[index] == '+' || chars[index] == '-')
                    && !matches!(chars[index - 1], 'e' | 'E')
                {
                    break;
                }
                index += 1;
            }
            tokens.push(Located {
                token: Token::Number(
                    chars[start..index]
                        .iter()
                        .collect::<String>()
                        .to_ascii_lowercase(),
                ),
                line,
            });
            continue;
        }
        tokens.push(Located {
            token: Token::Punct(ch),
            line,
        });
        index += 1;
    }
    tokens
}

const TYPE_ALIASES: &[(&str, &str)] = &[
    ("float2", "v2"),
    ("float3", "v3"),
    ("float4", "v4"),
    ("vec2", "v2"),
    ("vec3", "v3"),
    ("vec4", "v4"),
    ("uint4", "u4"),
    ("uvec4", "u4"),
    ("ivec4", "u4"),
    ("float", "f"),
    ("uint", "u"),
    ("int", "i"),
    ("dfdx", "ddx"),
    ("dfdy", "ddy"),
    ("lerp", "mix"),
];

const DROPPED_KEYWORDS: &[&str] = &["inline", "static", "const", "constant"];

fn alias(word: &str) -> &str {
    TYPE_ALIASES
        .iter()
        .find(|(from, _)| *from == word)
        .map_or(word, |(_, to)| *to)
}

fn normalize(tokens: &[Located]) -> Vec<Located> {
    let mut first: Vec<Located> = Vec::with_capacity(tokens.len());
    let mut index = 0usize;
    while index < tokens.len() {
        if let Token::Ident(word) = &tokens[index].token
            && word == "root"
            && matches!(
                tokens.get(index + 1).map(|t| &t.token),
                Some(Token::Punct('.'))
            )
        {
            index += 2;
            continue;
        }
        first.push(tokens[index].clone());
        index += 1;
    }
    let mut second: Vec<Located> = Vec::with_capacity(first.len());
    for located in first {
        match &located.token {
            Token::Ident(word) if DROPPED_KEYWORDS.contains(&word.as_str()) => continue,
            Token::Ident(word) => second.push(Located {
                token: Token::Ident(alias(word).to_owned()),
                line: located.line,
            }),
            other => second.push(Located {
                token: other.clone(),
                line: located.line,
            }),
        }
    }
    drop_broadcast_constructors(&second)
}

fn drop_broadcast_constructors(tokens: &[Located]) -> Vec<Located> {
    let mut drop = vec![false; tokens.len()];
    let mut index = 0usize;
    while index + 1 < tokens.len() {
        let is_ctor = matches!(&tokens[index].token, Token::Ident(word)
            if matches!(word.as_str(), "v2" | "v3" | "v4" | "u4" | "f" | "u" | "i"));
        if is_ctor && matches!(tokens[index + 1].token, Token::Punct('(')) {
            let mut depth = 0i32;
            let mut close = None;
            let mut has_comma = false;
            let mut cursor = index + 1;
            while cursor < tokens.len() {
                match tokens[cursor].token {
                    Token::Punct('(') => depth += 1,
                    Token::Punct(')') => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(cursor);
                            break;
                        }
                    }
                    Token::Punct(',') if depth == 1 => has_comma = true,
                    _ => {}
                }
                cursor += 1;
            }
            if let Some(close) = close
                && !has_comma
                && close > index + 1
            {
                drop[index] = true;
                drop[index + 1] = true;
                drop[close] = true;
                index = close + 1;
                continue;
            }
        }
        index += 1;
    }
    tokens
        .iter()
        .enumerate()
        .filter(|(position, _)| !drop[*position])
        .map(|(_, located)| located.clone())
        .collect()
}

fn statements(tokens: &[Located]) -> Vec<Statement> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start_line = tokens.first().map_or(0, |located| located.line);
    for located in tokens {
        match &located.token {
            Token::Punct(';') | Token::Punct('{') | Token::Punct('}') => {
                let text = current.trim().to_owned();
                if !text.is_empty() {
                    out.push(Statement {
                        line: start_line,
                        text,
                    });
                }
                current.clear();
                start_line = located.line;
            }
            token => {
                if current.is_empty() {
                    start_line = located.line;
                }
                push_token(&mut current, token);
            }
        }
    }
    let text = current.trim().to_owned();
    if !text.is_empty() {
        out.push(Statement {
            line: start_line,
            text,
        });
    }
    out
}

fn push_token(out: &mut String, token: &Token) {
    let needs_space = match token {
        Token::Punct(_) => false,
        _ => out.chars().next_back().is_some_and(|last| last != '('),
    };
    if needs_space && !out.is_empty() {
        out.push(' ');
    }
    match token {
        Token::Ident(word) | Token::Number(word) => out.push_str(word),
        Token::Punct(ch) => out.push(*ch),
    }
}

pub fn normalized_body(source: &ShaderSource, name: &str) -> Result<Vec<Statement>, String> {
    let mut last_error = String::new();
    for (file, contents) in &source.files {
        let stripped = strip_line_comments(contents);
        let tokens = tokenize(&stripped);
        let mut index = 0usize;
        while index < tokens.len() {
            let matches_name = matches!(&tokens[index].token, Token::Ident(word) if word == name);
            if matches_name {
                let mut cursor = index;
                while cursor < tokens.len() && !matches!(tokens[cursor].token, Token::Punct('{')) {
                    cursor += 1;
                }
                if cursor == tokens.len() {
                    last_error = format!(
                        "{}: 函数 `{name}` 在 {file} 里没有函数体",
                        source.language.name()
                    );
                    break;
                }
                let mut depth = 0i32;
                let mut end = None;
                for (offset, located) in tokens[cursor..].iter().enumerate() {
                    match located.token {
                        Token::Punct('{') => depth += 1,
                        Token::Punct('}') => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(cursor + offset);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let Some(end) = end else {
                    last_error = format!(
                        "{}: 函数 `{name}` 的花括号不配对（{file}）",
                        source.language.name()
                    );
                    break;
                };
                let body = normalize(&tokens[cursor + 1..end]);
                return Ok(statements(&body));
            }
            index += 1;
        }
    }
    if last_error.is_empty() {
        Err(format!(
            "{}: 找不到函数 `{name}`（已扫描 {:?}）",
            source.language.name(),
            source
                .files
                .iter()
                .map(|(file, _)| file.as_str())
                .collect::<Vec<_>>()
        ))
    } else {
        Err(last_error)
    }
}

pub const MATH_FUNCTIONS: &[&str] = &[
    "sd_rounded_rect",
    "erf_approx",
    "shadow_cdf",
    "analytic_shadow",
    "gradient_eval",
    "geometry_world",
    "aa_coverage",
    "clip_mask",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mismatch {
    pub function: String,
    pub expected_language: &'static str,
    pub expected_file: String,
    pub expected_line: usize,
    pub expected: String,
    pub actual_language: &'static str,
    pub actual_file: String,
    pub actual_line: usize,
    pub actual: String,
    pub statement_index: usize,
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "\n  函数:       {}（第 {} 条语句）\n  期望:       {}:{}  `{}`\n  实际:       {}:{}  `{}`",
            self.function,
            self.statement_index + 1,
            self.expected_file,
            self.expected_line,
            self.expected,
            self.actual_file,
            self.actual_line,
            self.actual
        )
    }
}

pub fn compare_function(
    expected: &ShaderSource,
    actual: &ShaderSource,
    name: &str,
) -> Result<(), Mismatch> {
    let left = normalized_body(expected, name).map_err(|detail| Mismatch {
        function: name.to_owned(),
        expected_language: expected.language.name(),
        expected_file: detail.clone(),
        expected_line: 0,
        expected: detail,
        actual_language: actual.language.name(),
        actual_file: String::new(),
        actual_line: 0,
        actual: String::new(),
        statement_index: 0,
    })?;
    let right = normalized_body(actual, name).map_err(|detail| Mismatch {
        function: name.to_owned(),
        expected_language: expected.language.name(),
        expected_file: String::new(),
        expected_line: 0,
        expected: String::new(),
        actual_language: actual.language.name(),
        actual_file: detail.clone(),
        actual_line: 0,
        actual: detail,
        statement_index: 0,
    })?;
    for index in 0..left.len().max(right.len()) {
        let expected_statement = left.get(index);
        let actual_statement = right.get(index);
        let same = match (expected_statement, actual_statement) {
            (Some(left), Some(right)) => left.text == right.text,
            (None, None) => true,
            _ => false,
        };
        if !same {
            return Err(Mismatch {
                function: name.to_owned(),
                expected_language: expected.language.name(),
                expected_file: expected
                    .files
                    .first()
                    .map_or_else(String::new, |(file, _)| file.clone()),
                expected_line: expected_statement.map_or(0, |statement| statement.line),
                expected: expected_statement.map_or_else(
                    || "<缺失（左侧语句更少）>".to_owned(),
                    |statement| statement.text.clone(),
                ),
                actual_language: actual.language.name(),
                actual_file: actual
                    .files
                    .first()
                    .map_or_else(String::new, |(file, _)| file.clone()),
                actual_line: actual_statement.map_or(0, |statement| statement.line),
                actual: actual_statement.map_or_else(
                    || "<缺失（右侧语句更少）>".to_owned(),
                    |statement| statement.text.clone(),
                ),
                statement_index: index,
            });
        }
    }
    Ok(())
}

pub fn compare_math_functions(
    reference: &ShaderSource,
    others: &[&ShaderSource],
) -> Result<(), Mismatch> {
    for name in MATH_FUNCTIONS {
        for other in others {
            compare_function(reference, other, name)?;
        }
    }
    Ok(())
}

pub fn check_anchors(file: &str, contents: &str, anchors: &[(&str, &str)]) -> Result<(), String> {
    for (what, expected) in anchors {
        if !contents.contains(expected) {
            return Err(format!(
                "{file}: {what} 应为 `{expected}`（找不到；三端渲染状态必须一致）"
            ));
        }
    }
    Ok(())
}

pub fn check_forbidden(
    file: &str,
    contents: &str,
    forbidden: &[(&str, &str)],
) -> Result<(), String> {
    for (what, token) in forbidden {
        if contents.contains(token) {
            return Err(format!("{file}: {what} 不得出现 `{token}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Language, ShaderSource, check_anchors, check_forbidden, compare_function,
        compare_math_functions, normalized_body, strip_line_comments,
    };

    fn hlsl(body: &str) -> ShaderSource {
        ShaderSource::new(
            Language::Hlsl,
            vec![("common.hlsl".to_owned(), body.to_owned())],
        )
    }

    fn glsl(body: &str) -> ShaderSource {
        ShaderSource::new(
            Language::Glsl,
            vec![("common.frag.glsl".to_owned(), body.to_owned())],
        )
    }

    fn metal(body: &str) -> ShaderSource {
        ShaderSource::new(
            Language::Metal,
            vec![("common.metal".to_owned(), body.to_owned())],
        )
    }

    #[test]
    fn stripping_comments_keeps_line_numbers() {
        let src = "a; // tail\nb;\n";
        let stripped = strip_line_comments(src);
        assert_eq!(stripped.matches('\n').count(), 2);
        assert!(!stripped.contains("tail"));
    }

    #[test]
    fn spelling_differences_normalize_away() {
        let left = hlsl(
            "float aa(float d) {\n  float w = max(fwidth(d), 1e-5);\n  return saturate(0.5 - d / w);\n}\n",
        );
        let right = glsl(
            "float aa(float d) {\n  float w = max(fwidth(d), 1e-5);\n  return saturate(0.5 - d / w);\n}\n",
        );
        assert!(compare_function(&left, &right, "aa").is_ok());

        let left = hlsl(
            "float f(float2 p, float2 h) {\n  float2 q = abs(p) - h + 2.0;\n  return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - 2.0;\n}\n",
        );
        let right = glsl(
            "float f(vec2 p, vec2 h) {\n`  vec2 q = abs(p) - h + 2.0;\n  return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - 2.0;\n}\n".replace('`', "").as_str(),
        );
        assert!(
            compare_function(&left, &right, "f").is_ok(),
            "float2/vec2 与 vec2(0.0)/0.0 必须归一化后相同"
        );

        let left = hlsl("float g() {\n  return frame.params.x;\n}\n");
        let right = glsl("float g() {\n  return root.frame.params.x;\n}\n");
        assert!(
            compare_function(&left, &right, "g").is_ok(),
            "`root.` 前缀必须被抹平"
        );
        let msl = metal("inline float g() {\n  return root.frame.params.x;\n}\n");
        assert!(compare_function(&left, &msl, "g").is_ok());
    }

    #[test]
    fn equivalent_rewrites_normalize_away() {
        let hlsl = hlsl(
            "float f(float t, float2 a, float2 b, float d) {\n  float w = length(float2(ddx(d), ddy(d)));\n  return lerp(a, b, t).x + mix(t, w, 0.5);\n}\n",
        );
        let glsl = glsl(
            "float f(float t, vec2 a, vec2 b, float d) {\n  float w = length(vec2(dFdx(d), dFdy(d)));\n  return mix(a, b, t).x + lerp(t, w, 0.5);\n}\n",
        );
        assert!(
            compare_function(&hlsl, &glsl, "f").is_ok(),
            "lerp/mix 与 ddx/dfdx 是等价改写，归一化后必须相同"
        );
        let msl = metal(
            "inline float f(float t, float2 a, float2 b, float d) {\n  float w = length(float2(dfdx(d), dfdy(d)));\n  return lerp(a, b, t).x + mix(t, w, 0.5);\n}\n",
        );
        assert!(compare_function(&hlsl, &msl, "f").is_ok());
    }

    #[test]
    fn the_math_contract_keeps_the_two_sided_shadow_and_the_scissor_clip() {
        let source = ShaderSource::new(
            Language::Glsl,
            vec![
                (
                    "common.glsl".to_owned(),
                    include_str!("shaders/common.glsl").to_owned(),
                ),
                (
                    "common.frag.glsl".to_owned(),
                    include_str!("shaders/common.frag.glsl").to_owned(),
                ),
            ],
        );

        let shadow: Vec<String> = normalized_body(&source, "analytic_shadow")
            .expect("analytic_shadow 必须可解析")
            .into_iter()
            .map(|statement| statement.text)
            .collect();
        let body = shadow.join("; ");
        assert_eq!(
            body.matches("shadow_cdf(").count(),
            5,
            "可分离高斯必须包含两条对边项（每轴 2 条 + SDF 1 条）：{body}"
        );
        assert_eq!(
            body.matches("+ half_size").count(),
            2,
            "两条对边项（x/y 轴各一条）必须存在：{body}"
        );

        let clip: Vec<String> = normalized_body(&source, "clip_mask")
            .expect("clip_mask 必须可解析")
            .into_iter()
            .map(|statement| statement.text)
            .collect();
        let body = clip.join("; ");
        assert!(
            body.contains("kytrix_clip_kind_rounded"),
            "clip_mask 必须按 meta.y 区分矩形 / 圆角项（标识符在归一化时小写）：{body}"
        );
        assert_eq!(
            body.matches("aa_coverage(").count(),
            1,
            "aa_coverage 只能乘一次（矩形项是 0/1 scissor）：{body}"
        );
        assert!(
            body.contains("floor(world_px)"),
            "矩形项必须用像素索引 floor(world_px) 判定（不得直接比 world_px）：{body}"
        );
        assert_eq!(
            body.matches("0.5").count(),
            1,
            "矩形项不得再用 ±0.5 px 的浮点像素单元比较（只剩圆角的 half_size 一处）：{body}"
        );

        let aa: Vec<String> = normalized_body(&source, "aa_coverage")
            .expect("aa_coverage 必须可解析")
            .into_iter()
            .map(|statement| statement.text)
            .collect();
        let body = aa.join("; ");
        assert!(
            body.contains("length(v2(ddx(d), ddy(d)))"),
            "AA 带宽必须是梯度模长：{body}"
        );
    }

    #[test]
    fn a_changed_constant_is_reported_with_the_first_difference() {
        let reference = hlsl(
            "float shadow(float d, float blur) {\n  float sigma = max(blur / 2.0, 0.35);\n  return 0.5 * (1.0 - erf(d / (sigma * 1.41421356)));\n}\n",
        );
        let mutated = glsl(
            "float shadow(float d, float blur) {\n  float sigma = max(blur / 3.0, 0.35);\n  return 0.5 * (1.0 - erf(d / (sigma * 1.41421356)));\n}\n",
        );
        let error = compare_function(&reference, &mutated, "shadow").expect_err("必须失败");
        assert_eq!(error.function, "shadow");
        assert_eq!(error.statement_index, 0);
        assert!(error.expected.contains("2.0"), "{error}");
        assert!(error.actual.contains("3.0"), "{error}");
        assert_eq!(error.expected_line, 2, "行号必须指向源文件的第 2 行");
        let text = error.to_string();
        assert!(text.contains("common.hlsl:2"), "{text}");
        assert!(text.contains("common.frag.glsl:2"), "{text}");
    }

    #[test]
    fn a_missing_statement_is_reported() {
        let reference =
            hlsl("float f() {\n  float a = 1.0;\n  float b = 2.0;\n  return a + b;\n}\n");
        let mutated = glsl("float f() {\n  float a = 1.0;\n  return a;\n}\n");
        let error = compare_function(&reference, &mutated, "f").expect_err("必须失败");
        assert_eq!(error.statement_index, 1);
    }

    #[test]
    fn a_missing_function_is_an_error() {
        let source = hlsl("float f() {\n  return 1.0;\n}\n");
        assert!(normalized_body(&source, "sd_rounded_rect").is_err());
        let error = compare_math_functions(&source, &[&source]).expect_err("必须失败");
        assert_eq!(error.function, "sd_rounded_rect");
    }

    #[test]
    fn render_state_anchors_must_all_be_present() {
        let windows = "SrcBlend: D3D12_BLEND_ONE, DestBlend: D3D12_BLEND_INV_SRC_ALPHA";
        let anchors = [
            ("blend src", "D3D12_BLEND_ONE"),
            ("blend dst", "D3D12_BLEND_INV_SRC_ALPHA"),
        ];
        assert!(check_anchors("windows/pipeline.rs", windows, &anchors).is_ok());
        let error = check_anchors(
            "windows/pipeline.rs",
            windows,
            &[("blend op", "D3D12_BLEND_OP_ADD")],
        )
        .expect_err("必须失败");
        assert!(error.contains("D3D12_BLEND_OP_ADD"), "{error}");
        let macos = "attachment.setBlendingEnabled(true);\nencoder.setCullMode(MTLCullMode::Back);";
        let forbidden = [("cull", "setCullMode")];
        assert!(check_forbidden("macos/pipeline.rs", macos, &forbidden).is_err());
        assert!(
            check_forbidden("macos/pipeline.rs", "setBlendingEnabled(true);", &forbidden).is_ok()
        );
    }
}
