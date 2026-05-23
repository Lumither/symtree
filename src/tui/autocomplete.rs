use super::App;

const PREDICATE_KEYS: &[&str] = &["lang:", "kind:", "file:", "name:"];
const KNOWN_KINDS: &[&str] = &[
    "fn", "method", "struct", "enum", "trait", "impl", "obj", "field", "mod", "const", "var",
    "array", "str", "bool", "file",
];
const COMMANDS: &[&str] = &[
    "help",
    "keymap",
    "warnings",
    "lsp",
    #[cfg(feature = "debug_perf")]
    "perf",
    "collapse",
    "expand",
    "q",
    "quit",
    "query",
];

pub(super) fn current_token(input: &str) -> &str {
    if let Some(idx) = input.rfind(char::is_whitespace) {
        &input[idx + 1..]
    } else {
        input
    }
}

pub(super) fn replace_last_token(input: &str, candidate: &str) -> String {
    let prefix_end = input.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let mut out = String::with_capacity(prefix_end + candidate.len());
    out.push_str(&input[..prefix_end]);
    out.push_str(candidate);
    out
}

pub(super) fn search_candidates(app: &App) -> Vec<String> {
    query_candidates(&app.filter, app)
}

pub(super) fn command_candidates(app: &App) -> Vec<String> {
    let input = app.command.as_str();

    if let Some(rest) = input.strip_prefix("query ") {
        let token = current_token(rest);
        let mut out: Vec<String> = Vec::new();
        if "clear".starts_with(token) {
            out.push("clear".to_string());
        }
        out.extend(query_candidates(rest, app));
        return out;
    }

    #[cfg(feature = "debug_perf")]
    if let Some(rest) = input.strip_prefix("perf ") {
        let token = current_token(rest);
        return ["reset"]
            .iter()
            .filter(|s| token.is_empty() || s.starts_with(token))
            .map(|s| s.to_string())
            .collect();
    }

    COMMANDS
        .iter()
        .filter(|c| input.is_empty() || c.starts_with(input))
        .map(|c| c.to_string())
        .collect()
}

fn query_candidates(input: &str, app: &App) -> Vec<String> {
    let token = current_token(input);
    if token.is_empty() {
        return PREDICATE_KEYS.iter().map(|s| s.to_string()).collect();
    }
    let (prefix, body) = match token.strip_prefix('!') {
        Some(rest) => ("!", rest),
        None => ("", token),
    };
    if let Some((key, value)) = body.split_once(':') {
        return value_candidates(key, value, app)
            .into_iter()
            .map(|c| format!("{prefix}{c}"))
            .collect();
    }
    let body_lower = body.to_lowercase();
    let mut out: Vec<String> = PREDICATE_KEYS
        .iter()
        .filter(|k| k.starts_with(&body_lower))
        .map(|s| format!("{prefix}{s}"))
        .collect();
    out.extend(
        symbol_names(app, &body_lower)
            .into_iter()
            .take(10)
            .map(|n| format!("{prefix}{n}")),
    );
    out
}

fn value_candidates(key: &str, partial: &str, app: &App) -> Vec<String> {
    let partial_lower = partial.to_lowercase();
    match key {
        "lang" => crate::languages::REGISTRY
            .keys()
            .filter(|k| k.to_lowercase().starts_with(&partial_lower))
            .map(|k| format!("lang:{k}"))
            .collect(),
        "kind" => KNOWN_KINDS
            .iter()
            .filter(|k| k.starts_with(&partial_lower))
            .map(|k| format!("kind:{k}"))
            .collect(),
        "name" => symbol_names(app, &partial_lower)
            .into_iter()
            .take(20)
            .map(|s| format!("name:{s}"))
            .collect(),
        "file" => Vec::new(),
        _ => Vec::new(),
    }
}

fn symbol_names(app: &App, partial: &str) -> Vec<String> {
    let visible = app.visible_nodes();
    let mut out = Vec::new();
    for vn in visible.iter() {
        if let Some(node) = crate::tree::get_node(&app.project.files, &vn.path)
            && node.name.to_lowercase().contains(partial)
        {
            out.push(node.name.clone());
            if out.len() >= 20 {
                break;
            }
        }
    }
    out
}
