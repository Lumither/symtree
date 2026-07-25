use regex::Regex;

#[derive(Debug, Clone)]
pub(crate) enum Expr {
    Or(Vec<Expr>),
    And(Vec<Expr>),
    Not(Box<Expr>),
    Predicate {
        key: PredicateKey,
        value: String,
    },
    Text {
        pattern: String,
        regex: Option<Regex>,
    },
}

/// Combine two optional queries with logical AND, treating `None` as "no
/// constraint". Used to fuse the persistent `:query` with the live `/filter`.
pub(crate) fn combine(a: Option<Expr>, b: Option<Expr>) -> Option<Expr> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Expr::And(vec![a, b])),
        (Some(expr), None) | (None, Some(expr)) => Some(expr),
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PredicateKey {
    Lang,
    Kind,
    File,
    Name,
}

impl PredicateKey {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "lang" => Some(Self::Lang),
            "kind" => Some(Self::Kind),
            "file" => Some(Self::File),
            "name" => Some(Self::Name),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Lang => "lang",
            Self::Kind => "kind",
            Self::File => "file",
            Self::Name => "name",
        }
    }
}

pub(crate) struct MatchCtx<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub file_path: &'a str,
    pub lang: Option<&'a str>,
}

impl Expr {
    pub(crate) fn matches(&self, ctx: &MatchCtx<'_>) -> bool {
        match self {
            Expr::Or(items) => items.iter().any(|e| e.matches(ctx)),
            Expr::And(items) => items.iter().all(|e| e.matches(ctx)),
            Expr::Not(inner) => !inner.matches(ctx),
            Expr::Predicate { key, value } => match key {
                PredicateKey::Lang => ctx.lang.is_some_and(|l| l.eq_ignore_ascii_case(value)),
                PredicateKey::Kind => ctx.kind.eq_ignore_ascii_case(value),
                PredicateKey::File => ctx.file_path.to_lowercase().contains(&value.to_lowercase()),
                PredicateKey::Name => ctx.name.to_lowercase().contains(&value.to_lowercase()),
            },
            Expr::Text { pattern, regex } => match regex {
                Some(re) => re.is_match(ctx.name),
                None => ctx.name.to_lowercase().contains(&pattern.to_lowercase()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token<'a> {
    Predicate {
        negated: bool,
        key: &'a str,
        value: &'a str,
    },
    Quoted {
        negated: bool,
        value: &'a str,
    },
    Word {
        negated: bool,
        value: &'a str,
    },
    Or,
}

pub(crate) fn tokenize(input: &str) -> Result<Vec<Token<'_>>, String> {
    let mut tokens = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'|' && bytes[i + 1] == b'|' {
            tokens.push(Token::Or);
            i += 2;
            continue;
        }
        let negated = b == b'!';
        if negated {
            i += 1;
            if i >= bytes.len() || bytes[i].is_ascii_whitespace() {
                return Err("dangling `!`".to_string());
            }
        }
        if i < bytes.len() && bytes[i] == b'"' {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end] != b'"' {
                end += 1;
            }
            if end >= bytes.len() {
                return Err("unterminated quoted string".to_string());
            }
            let value = &input[start..end];
            tokens.push(Token::Quoted { negated, value });
            i = end + 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            if i + 1 < bytes.len() && bytes[i] == b'|' && bytes[i + 1] == b'|' {
                break;
            }
            i += 1;
        }
        let slice = &input[start..i];
        if let Some((key, value)) = slice.split_once(':') {
            if value.is_empty() {
                return Err(format!("predicate `{key}:` is missing a value"));
            }
            tokens.push(Token::Predicate {
                negated,
                key,
                value,
            });
        } else {
            tokens.push(Token::Word {
                negated,
                value: slice,
            });
        }
    }
    Ok(tokens)
}

pub(crate) fn parse(input: &str) -> Result<Option<Expr>, String> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    parse_or(&tokens).map(Some)
}

fn parse_or(tokens: &[Token<'_>]) -> Result<Expr, String> {
    let mut groups: Vec<Vec<&Token>> = vec![Vec::new()];
    for tok in tokens {
        if matches!(tok, Token::Or) {
            if groups.last().is_some_and(|g| g.is_empty()) {
                return Err("empty operand before/after `||`".to_string());
            }
            groups.push(Vec::new());
        } else {
            groups.last_mut().unwrap().push(tok);
        }
    }
    if groups.last().is_some_and(|g| g.is_empty()) {
        return Err("empty operand after `||`".to_string());
    }
    let mut ands = Vec::with_capacity(groups.len());
    for group in groups {
        ands.push(parse_and(&group)?);
    }
    if ands.len() == 1 {
        Ok(ands.into_iter().next().unwrap())
    } else {
        Ok(Expr::Or(ands))
    }
}

fn parse_and(tokens: &[&Token<'_>]) -> Result<Expr, String> {
    if tokens.is_empty() {
        return Err("empty expression".to_string());
    }
    let mut atoms = Vec::with_capacity(tokens.len());
    for tok in tokens {
        atoms.push(parse_atom(tok)?);
    }
    if atoms.len() == 1 {
        Ok(atoms.into_iter().next().unwrap())
    } else {
        Ok(Expr::And(atoms))
    }
}

fn parse_atom(tok: &Token<'_>) -> Result<Expr, String> {
    let (negated, expr) = match tok {
        Token::Predicate {
            negated,
            key,
            value,
        } => {
            let parsed_key =
                PredicateKey::parse(key).ok_or_else(|| format!("unknown predicate `{key}:`"))?;
            (
                *negated,
                Expr::Predicate {
                    key: parsed_key,
                    value: (*value).to_string(),
                },
            )
        }
        Token::Quoted { negated, value } => {
            let re = Regex::new(&format!("(?i){value}"))
                .map_err(|e| format!("invalid regex `{value}`: {e}"))?;
            (
                *negated,
                Expr::Text {
                    pattern: (*value).to_string(),
                    regex: Some(re),
                },
            )
        }
        Token::Word { negated, value } => (
            *negated,
            Expr::Text {
                pattern: (*value).to_string(),
                regex: None,
            },
        ),
        Token::Or => return Err("unexpected `||`".to_string()),
    };
    Ok(if negated {
        Expr::Not(Box::new(expr))
    } else {
        expr
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(name: &'a str, kind: &'a str, file: &'a str, lang: Option<&'a str>) -> MatchCtx<'a> {
        MatchCtx {
            name,
            kind,
            file_path: file,
            lang,
        }
    }

    #[test]
    fn bare_word_matches_name_substring_case_insensitive() {
        let expr = parse("Render").unwrap().unwrap();
        assert!(expr.matches(&ctx("render_tree", "fn", "a.rs", None)));
        assert!(expr.matches(&ctx("HelloRender", "fn", "a.rs", None)));
        assert!(!expr.matches(&ctx("foo", "fn", "a.rs", None)));
    }

    #[test]
    fn predicate_kind_matches_exact() {
        let expr = parse("kind:fn").unwrap().unwrap();
        assert!(expr.matches(&ctx("x", "fn", "a.rs", None)));
        assert!(!expr.matches(&ctx("x", "struct", "a.rs", None)));
    }

    #[test]
    fn implicit_and_and_explicit_or() {
        let expr = parse("lang:rust kind:fn || kind:struct").unwrap().unwrap();
        assert!(expr.matches(&ctx("x", "fn", "a.rs", Some("rust"))));
        assert!(expr.matches(&ctx("x", "struct", "a.py", Some("python"))));
        assert!(!expr.matches(&ctx("x", "fn", "a.py", Some("python"))));
    }

    #[test]
    fn negation_predicate() {
        let expr = parse("!kind:variable").unwrap().unwrap();
        assert!(expr.matches(&ctx("x", "fn", "a.rs", None)));
        assert!(!expr.matches(&ctx("x", "variable", "a.rs", None)));
    }

    #[test]
    fn quoted_regex() {
        let expr = parse("\"^test_.*\"").unwrap().unwrap();
        assert!(expr.matches(&ctx("test_foo", "fn", "a.rs", None)));
        assert!(!expr.matches(&ctx("foo_test", "fn", "a.rs", None)));
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(parse("   ").unwrap().is_none());
    }

    #[test]
    fn unknown_predicate_errors() {
        assert!(parse("foo:bar").is_err());
    }

    #[test]
    fn dangling_negation_errors() {
        assert!(parse("! lang:rust").is_err());
    }

    #[test]
    fn missing_value_errors() {
        assert!(parse("lang:").is_err());
    }
}
