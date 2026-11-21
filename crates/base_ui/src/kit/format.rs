//! 数字格式化：分位（千分位）分组（旧 `crate::format` 的引擎化移植）。
//!
//! 纯文本、无主题、无窗口：输入是 `market_participants_model` 写出来的数字串（数字、可选负号、
//! 可选小数、`e` 记数法），输出是只在**整数部分**插入逗号的等值文本。旧实现与迁移后实现必须
//! 逐值等价（本模块的测试直接对旧函数现场比对，不硬编码期望值）。

use smol_str::SmolStr;

/// 给数字串的整数部分加千分位逗号。
///
/// 语义（与旧 `crate::format::group_digits` 逐字节等价）：
/// - 只处理可选前导 `-` 之后、第一个 `.` / `e` / `E` 之前的**整数部分**；
/// - 整数部分长度 `<= 3` 或含非 ASCII 数字字符时**原样返回**（不去猜；`--`、`1,234`、`+12` 等
///   输入都保持原样）；
/// - 其余部分（符号 / 小数 / 指数）原样搬过去，绝不改变数值含义。
///
/// 返回 `SmolStr`（短数字零分配堆；AGENTS §2.2）。
#[must_use]
pub fn group_digits(text: &str) -> SmolStr {
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", text),
    };
    let (integer, tail) = match rest.find(['.', 'e', 'E']) {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    if integer.len() <= 3 || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
        return SmolStr::new(text);
    }
    let mut grouped = String::with_capacity(text.len() + integer.len() / 3);
    grouped.push_str(sign);
    for (index, digit) in integer.bytes().enumerate() {
        if index > 0 && (integer.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(char::from(digit));
    }
    grouped.push_str(tail);
    SmolStr::new(grouped)
}

#[cfg(test)]
mod tests {
    use super::group_digits;

    /// 手工样例：正负号 / 小数 / 指数 / 非法输入的语义与旧实现一致。
    #[test]
    fn hand_picked_numbers_are_grouped_like_the_legacy_implementation() {
        let cases = [
            "",
            "0",
            "999",
            "1000",
            "1234",
            "1234567.00",
            "-12345.5",
            "-999",
            "-0.5",
            "1.50e18",
            "1e5",
            "12e3",
            "1234e5",
            "--",
            "abc",
            "1,234",
            "+1234",
            "12 34",
            "12345678901234567890",
            "1234.5678",
        ];
        for input in cases {
            assert_eq!(
                group_digits(input).as_str(),
                crate::format::group_digits(input).as_str(),
                "输入 `{input}` 的分组结果与旧实现不一致"
            );
        }
        assert_eq!(group_digits("1234567.00"), "1,234,567.00");
        assert_eq!(group_digits("-12345.5"), "-12,345.5");
        assert_eq!(group_digits("1.50e18"), "1.50e18");
        assert_eq!(group_digits(""), "");
    }

    /// 抽样 ≥ 20 组程序生成的数字串，逐位与旧实现等价（覆盖位数 / 符号 / 小数 / 指数组合）。
    #[test]
    fn generated_numbers_match_the_legacy_implementation() {
        let mut checked = 0usize;
        for digits in 1..=8u32 {
            let integer = 10u64.pow(digits) - 1;
            let input = format!("{integer}");
            assert_eq!(group_digits(&input), crate::format::group_digits(&input));
            checked += 1;
            for suffix in [".5", ".00", "e3", "e-7"] {
                let input = format!("{integer}{suffix}");
                assert_eq!(group_digits(&input), crate::format::group_digits(&input));
                checked += 1;
                let negative = format!("-{integer}{suffix}");
                assert_eq!(
                    group_digits(&negative),
                    crate::format::group_digits(&negative)
                );
                checked += 1;
            }
        }
        assert!(checked >= 20, "抽样数必须 ≥ 20（实际 {checked}）");
    }
}
