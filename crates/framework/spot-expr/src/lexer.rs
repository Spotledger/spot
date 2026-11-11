//! The lexer (specification section 2).

use rust_decimal::Decimal;

use crate::error::{ParseError, codes};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok {
    Int(i64),
    Dec(Decimal),
    Text(String),
    Ident(String),
    And,
    Or,
    Not,
    In,
    True,
    False,
    Null,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Question,
    Colon,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    End,
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub tok: Tok,
    /// Character offset of the first character.
    pub offset: usize,
}

const MAX_DIGITS: usize = 28;

/// Builds a decimal from the digits before and after the point; `None` when it needs more than
/// 28 significant digits or a scale above 28.
pub(crate) fn decimal_from_digits(int_part: &str, frac_part: &str) -> Option<Decimal> {
    if frac_part.len() > MAX_DIGITS {
        return None;
    }
    let all = format!("{int_part}{frac_part}");
    let significant = all.trim_start_matches('0').len();
    if significant > MAX_DIGITS {
        return None;
    }
    let mantissa: i128 = if all.is_empty() { 0 } else { all.parse().ok()? };
    Some(Decimal::from_i128_with_scale(
        mantissa,
        u32::try_from(frac_part.len()).ok()?,
    ))
}

pub(crate) fn lex(source: &str) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            i += 1;
            continue;
        }
        let tok = if c.is_ascii_digit() {
            let int_start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let int_part: String = chars[int_start..i].iter().collect();
            let has_fraction =
                chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if has_fraction {
                i += 1;
                let frac_start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let frac: String = chars[frac_start..i].iter().collect();
                match decimal_from_digits(&int_part, &frac) {
                    Some(d) => Tok::Dec(d),
                    None => {
                        return Err(ParseError::new(
                            codes::NUMBER_RANGE,
                            start,
                            "a decimal literal has at most 28 significant digits and scale 28",
                        ));
                    }
                }
            } else {
                match int_part.parse::<i64>() {
                    Ok(n) => Tok::Int(n),
                    Err(_) => {
                        return Err(ParseError::new(
                            codes::NUMBER_RANGE,
                            start,
                            "an integer literal must fit in 64 bits",
                        ));
                    }
                }
            }
        } else if c.is_ascii_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "and" => Tok::And,
                "or" => Tok::Or,
                "not" => Tok::Not,
                "in" => Tok::In,
                "true" => Tok::True,
                "false" => Tok::False,
                "null" => Tok::Null,
                _ => Tok::Ident(word),
            }
        } else if c == '\'' || c == '"' {
            i += 1;
            let mut text = String::new();
            loop {
                let Some(&ch) = chars.get(i) else {
                    return Err(ParseError::new(
                        codes::UNTERMINATED_STRING,
                        start,
                        "a text literal is not closed",
                    ));
                };
                i += 1;
                if ch == c {
                    break;
                }
                if ch == '\\' {
                    let Some(&esc) = chars.get(i) else {
                        return Err(ParseError::new(
                            codes::UNTERMINATED_STRING,
                            start,
                            "a text literal is not closed",
                        ));
                    };
                    let decoded = match esc {
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        _ => {
                            return Err(ParseError::new(
                                codes::BAD_ESCAPE,
                                i - 1,
                                "unknown escape in a text literal",
                            ));
                        }
                    };
                    i += 1;
                    text.push(decoded);
                } else {
                    text.push(ch);
                }
            }
            Tok::Text(text)
        } else {
            i += 1;
            let next = chars.get(i).copied();
            let two = |second: char, yes: Tok, no: Option<Tok>, i: &mut usize| -> Option<Tok> {
                if next == Some(second) {
                    *i += 1;
                    Some(yes)
                } else {
                    no
                }
            };
            let tok = match c {
                '(' => Some(Tok::LParen),
                ')' => Some(Tok::RParen),
                '[' => Some(Tok::LBracket),
                ']' => Some(Tok::RBracket),
                ',' => Some(Tok::Comma),
                '.' => Some(Tok::Dot),
                '?' => Some(Tok::Question),
                ':' => Some(Tok::Colon),
                '+' => Some(Tok::Plus),
                '-' => Some(Tok::Minus),
                '*' => Some(Tok::Star),
                '/' => Some(Tok::Slash),
                '%' => Some(Tok::Percent),
                '=' => two('=', Tok::EqEq, None, &mut i),
                '!' => two('=', Tok::NotEq, None, &mut i),
                '<' => two('=', Tok::Le, Some(Tok::Lt), &mut i),
                '>' => two('=', Tok::Ge, Some(Tok::Gt), &mut i),
                _ => None,
            };
            match tok {
                Some(t) => t,
                None => {
                    return Err(ParseError::new(
                        codes::SYNTAX,
                        start,
                        format!("unexpected character `{c}`"),
                    ));
                }
            }
        };
        tokens.push(Token { tok, offset: start });
    }
    tokens.push(Token {
        tok: Tok::End,
        offset: chars.len(),
    });
    Ok(tokens)
}
