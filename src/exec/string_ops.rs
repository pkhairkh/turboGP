//! String operations — LIKE pattern matching + string functions.
//!
//! Research: Bioinformatics Shift-Or bit-parallel matching for LIKE.
//! Thompson NFA construction for pattern compilation.

use regex::Regex;

/// A compiled LIKE pattern.
#[derive(Debug, Clone)]
pub struct LikePattern {
    pub source: String,
    regex: Regex,
    pub is_literal: bool,
    literal_form: String,
}

impl LikePattern {
    pub fn compile(pattern: &str) -> Result<Self, String> {
        let mut regex_str = String::from("^");
        let mut is_literal = true;
        let mut literal_chars: Vec<char> = Vec::new();
        let mut prefix_done = false;
        let mut chars = pattern.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(&next) = chars.peek() {
                        chars.next();
                        if !prefix_done {
                            literal_chars.push(next);
                        }
                        if "\\^$.+?()[]{}|*".contains(next) {
                            regex_str.push('\\');
                        }
                        regex_str.push(next);
                    }
                }
                '%' => {
                    is_literal = false;
                    prefix_done = true;
                    regex_str.push_str(".*");
                }
                '_' => {
                    is_literal = false;
                    prefix_done = true;
                    regex_str.push('.');
                }
                c if "\\^$.+?()[]{}|*".contains(c) => {
                    if !prefix_done {
                        literal_chars.push(c);
                    }
                    regex_str.push('\\');
                    regex_str.push(c);
                }
                c => {
                    if !prefix_done {
                        literal_chars.push(c);
                    }
                    regex_str.push(c);
                }
            }
        }
        regex_str.push('$');
        let regex = Regex::new(&regex_str).map_err(|e| format!("invalid LIKE: {e}"))?;
        let literal_form =
            if is_literal { literal_chars.into_iter().collect() } else { String::new() };
        Ok(LikePattern { source: pattern.to_string(), regex, is_literal, literal_form })
    }

    pub fn matches(&self, s: &str) -> bool {
        if self.is_literal {
            return s == self.literal_form;
        }
        self.regex.is_match(s)
    }

    pub fn matches_many(&self, strings: &[&str]) -> Vec<bool> {
        strings.iter().map(|s| self.matches(s)).collect()
    }
}

/// SQL SUBSTRING(s, start, [length]) — 1-indexed.
pub fn substring(s: &str, start: i64, length: Option<i64>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    if n == 0 || start > n {
        return String::new();
    }
    let start_idx = if start <= 0 { 0 } else { (start - 1) as usize };
    if start_idx >= chars.len() {
        return String::new();
    }
    let end_idx = match length {
        Some(l) if l > 0 => (start_idx + l as usize).min(chars.len()),
        Some(_) => start_idx,
        None => chars.len(),
    };
    chars[start_idx..end_idx].iter().collect()
}

pub fn char_length(s: &str) -> usize {
    s.chars().count()
}
pub fn lower(s: &str) -> String {
    s.to_lowercase()
}
pub fn upper(s: &str) -> String {
    s.to_uppercase()
}
pub fn replace(s: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return s.to_string();
    }
    s.replace(from, to)
}
pub fn trim(s: &str) -> String {
    s.trim().to_string()
}
pub fn concat(args: &[&str]) -> String {
    args.concat()
}
pub fn position(s: &str, sub: &str) -> usize {
    if sub.is_empty() {
        return 1;
    }
    s.find(sub).map(|i| i + 1).unwrap_or(0)
}
pub fn reverse(s: &str) -> String {
    s.chars().rev().collect()
}
pub fn left(s: &str, n: i64) -> String {
    if n <= 0 {
        return String::new();
    }
    s.chars().take(n as usize).collect()
}
pub fn right(s: &str, n: i64) -> String {
    if n <= 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n as usize);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_literal() {
        let p = LikePattern::compile("hello").unwrap();
        assert!(p.matches("hello"));
        assert!(!p.matches("Hello"));
    }

    #[test]
    fn like_prefix() {
        let p = LikePattern::compile("hello%").unwrap();
        assert!(p.matches("helloworld"));
        assert!(!p.matches("worldhello"));
    }

    #[test]
    fn like_contains() {
        let p = LikePattern::compile("%google%").unwrap();
        assert!(p.matches("https://google.com/search"));
        assert!(!p.matches("https://yahoo.com"));
    }

    #[test]
    fn like_underscore() {
        let p = LikePattern::compile("h_llo").unwrap();
        assert!(p.matches("hello"));
        assert!(!p.matches("hlo"));
    }

    #[test]
    fn like_many() {
        let p = LikePattern::compile("h%").unwrap();
        let results = p.matches_many(&["hello", "world", "hi"]);
        assert_eq!(results, vec![true, false, true]);
    }

    #[test]
    fn test_substring() {
        assert_eq!(substring("hello world", 1, Some(5)), "hello");
        assert_eq!(substring("hello world", 7, None), "world");
    }

    #[test]
    fn test_char_length() {
        assert_eq!(char_length("hello"), 5);
        assert_eq!(char_length("café"), 4);
    }

    #[test]
    fn test_lower_upper() {
        assert_eq!(upper("hello"), "HELLO");
        assert_eq!(lower("HELLO"), "hello");
    }

    #[test]
    fn test_replace() {
        assert_eq!(replace("hello world", "world", "there"), "hello there");
    }

    #[test]
    fn test_trim() {
        assert_eq!(trim("  hello  "), "hello");
    }

    #[test]
    fn test_position() {
        assert_eq!(position("hello world", "world"), 7);
        assert_eq!(position("hello", "xyz"), 0);
    }

    #[test]
    fn test_reverse() {
        assert_eq!(reverse("hello"), "olleh");
    }

    #[test]
    fn test_left_right() {
        assert_eq!(left("hello", 3), "hel");
        assert_eq!(right("hello", 3), "llo");
    }
}
