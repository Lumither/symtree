use std::{
    error::Error,
    fmt,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DOCUMENT_SYMBOLS_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound on how long we retry `documentSymbol` while a server reports it is
/// still indexing the workspace (elp, rust-analyzer). Without this the first load
/// would be dropped entirely.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(120);
const SERVER_READY_POLL_INTERVAL: Duration = Duration::from_millis(300);
/// How long to wait for a reply to the `shutdown` request before giving up and
/// forcing the process down. Bounded so a server that never answers can't hang us.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for the process to exit on its own after `exit` before we kill it.
const EXIT_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const MEMORY_CHECK_INTERVAL: Duration = Duration::from_secs(3);
const MEMORY_ABSOLUTE_THRESHOLD: u64 = 5 * 1024 * 1024 * 1024;
const MEMORY_RATE_THRESHOLD: u64 = 500 * 1024 * 1024;

use serde_json::{Value, json};

use crate::{
    error::{AppContext, AppResult, app_error},
    index,
    languages::{LanguageDef, lsp_program},
    model::{SymbolKind, SymbolNode},
    project::collect_source_files,
};

/// An LSP `error` response plus whether it is worth retrying — set when the
/// server is merely not ready yet rather than refusing the request outright.
#[derive(Debug)]
struct LspError {
    message: String,
    retryable: bool,
}

impl fmt::Display for LspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for LspError {}

fn error_is_retryable(code: Option<i64>, message: &str) -> bool {
    // -32801 ContentModified, -32802 ServerCancelled, -32803 RequestFailed,
    // -32002 ServerNotInitialized — all "try again once I'm settled" signals.
    matches!(code, Some(-32801 | -32802 | -32803 | -32002)) || {
        let lower = message.to_lowercase();
        ["loading", "indexing", "not ready", "initializing"]
            .iter()
            .any(|needle| lower.contains(needle))
    }
}

pub(crate) enum LoadEvent {
    Started,
    Discovered(usize),
    FileLoaded(SymbolNode),
    /// The non-fatal arm of the error taxonomy: a per-file or per-language
    /// failure the load survives (a server that wouldn't start, a file that
    /// couldn't be read). Fatal failures instead short-circuit via `AppResult`.
    Warning(String),
    Finished,
}

pub(crate) fn stream_language(root: &Path, lang: &LanguageDef, tx: Sender<LoadEvent>) {
    let warn = |tx: &Sender<LoadEvent>, msg: String| {
        let _ = tx.send(LoadEvent::Warning(msg));
    };
    let lang_label = lsp_program(&lang.lsp).to_string();

    let files = match collect_source_files(root, &lang.extensions) {
        Ok(f) => f,
        Err(error) => {
            warn(&tx, format!("{lang_label}: {error}"));
            return;
        }
    };
    if files.is_empty() {
        return;
    }
    let _ = tx.send(LoadEvent::Discovered(files.len()));

    // Serve what the index knows; only spawn a server if something is stale.
    // Signatures are taken before the read, so an edit during the load
    // invalidates the shard we are about to write instead of being missed.
    let mut stale = Vec::new();
    for file in files {
        let relative_name = relative_path(root, &file);
        let signature = index::signature(&file);
        match signature.and_then(|sig| index::get(root, &relative_name, sig)) {
            Some(symbols) => {
                let _ = tx.send(LoadEvent::FileLoaded(SymbolNode::file(
                    relative_name,
                    symbols,
                )));
            }
            None => stale.push((file, relative_name, signature)),
        }
    }
    if stale.is_empty() {
        return;
    }

    let root_uri = file_uri(root);
    let root_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace")
        .to_string();

    let mut client = match LspClient::spawn(&lang.lsp, root_uri.clone(), root_name.clone()) {
        Ok(c) => c,
        Err(error) => {
            warn(&tx, format!("{lang_label}: {error}"));
            return;
        }
    };
    if let Err(error) = client.initialize(root, &root_uri, &root_name) {
        warn(&tx, format!("{lang_label}: initialize: {error}"));
        return;
    }

    let language_id = lang.language_id.as_deref().unwrap_or("plaintext");
    let lsp_pid = client.pid();
    let mut last_memory_check = Instant::now();
    let mut last_memory_rss: u64 = 0;
    let mut warned_absolute = false;
    let mut warned_rate = false;
    for (file, relative_name, signature) in stale {
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) => {
                warn(&tx, format!("{}: {error}", file.display()));
                continue;
            }
        };

        let uri = file_uri(&file);
        if let Err(error) = client.did_open(&uri, &text, language_id) {
            warn(&tx, format!("{relative_name}: did_open: {error}"));
            continue;
        }

        match client.document_symbols(&uri) {
            Ok(symbols) => {
                if let Some(signature) = signature
                    && let Err(error) = index::put(root, &relative_name, signature, &symbols)
                {
                    warn(&tx, format!("index: {error}"));
                }
                let _ = tx.send(LoadEvent::FileLoaded(SymbolNode::file(
                    relative_name,
                    symbols,
                )));
            }
            Err(error) => {
                warn(&tx, format!("{relative_name}: symbols: {error}"));
            }
        }

        if last_memory_check.elapsed() >= MEMORY_CHECK_INTERVAL
            && let Some(pid) = lsp_pid
            && let Some(rss) = process_tree_rss_bytes(pid)
        {
            if !warned_absolute && rss > MEMORY_ABSOLUTE_THRESHOLD {
                warn(
                    &tx,
                    format!(
                        "{lang_label}: high memory: {:.1} GiB",
                        rss as f64 / (1024.0 * 1024.0 * 1024.0)
                    ),
                );
                warned_absolute = true;
            }
            if !warned_rate && last_memory_rss > 0 {
                let delta = rss.saturating_sub(last_memory_rss);
                if delta > MEMORY_RATE_THRESHOLD {
                    warn(
                        &tx,
                        format!(
                            "{lang_label}: rapid memory growth: +{:.0} MiB in {:.0}s",
                            delta as f64 / (1024.0 * 1024.0),
                            last_memory_check.elapsed().as_secs_f64()
                        ),
                    );
                    warned_rate = true;
                }
            }
            last_memory_rss = rss;
            last_memory_check = Instant::now();
        }
    }

    client.shutdown();
}

fn process_tree_rss_bytes(root_pid: u32) -> Option<u64> {
    let pids = collect_tree_pids(root_pid);
    if pids.is_empty() {
        return None;
    }
    let pid_list: Vec<String> = pids.iter().map(u32::to_string).collect();
    let output = Command::new("ps")
        .arg("-o")
        .arg("rss=")
        .arg("-p")
        .arg(pid_list.join(","))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let total_kb: u64 = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u64>().ok())
        .sum();
    Some(total_kb.saturating_mul(1024))
}

fn collect_tree_pids(root: u32) -> Vec<u32> {
    let mut result = vec![root];
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for child in direct_children_pids(parent) {
            result.push(child);
            queue.push(child);
        }
    }
    result
}

fn direct_children_pids(parent: u32) -> Vec<u32> {
    let output = Command::new("pgrep")
        .arg("-P")
        .arg(parent.to_string())
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn lsp_is_available(command: &str) -> bool {
    let program = command.split_whitespace().next().unwrap_or(command);
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file();
    }
    lsp_search_path()
        .split(':')
        .filter(|p| !p.is_empty())
        .any(|dir| Path::new(dir).join(program).is_file())
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    incoming_rx: Receiver<AppResult<Value>>,
    _reader_handle: Option<JoinHandle<()>>,
    next_id: i64,
    root_uri: String,
    root_name: String,
}

impl LspClient {
    fn spawn(command: &str, root_uri: String, root_name: String) -> AppResult<Self> {
        let mut parts = command.split_whitespace();
        let program = parts.next().ok_or_else(|| app_error("empty LSP command"))?;
        let args: Vec<&str> = parts.collect();
        let resolved = resolve_lsp_command(program);
        let mut child = Command::new(&resolved)
            .args(&args)
            .env("PATH", lsp_search_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("failed to start LSP command `{command}`"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| app_error("failed to capture LSP stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| app_error("failed to capture LSP stdout"))?;

        let (tx, incoming_rx) = mpsc::channel::<AppResult<Value>>();
        let reader_handle = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_lsp_message(&mut reader) {
                    Ok(message) => {
                        if tx.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            stdin,
            incoming_rx,
            _reader_handle: Some(reader_handle),
            next_id: 1,
            root_uri,
            root_name,
        })
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn initialize(&mut self, root: &Path, root_uri: &str, root_name: &str) -> AppResult<()> {
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootPath": root.to_string_lossy(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "documentSymbol": {
                            "hierarchicalDocumentSymbolSupport": true,
                            "symbolKind": {
                                "valueSet": (1..=26).collect::<Vec<i64>>()
                            }
                        }
                    },
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true
                    }
                },
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": root_name
                }]
            }),
        )?;

        self.notify("initialized", json!({}))
    }

    fn did_open(&mut self, uri: &str, text: &str, language_id: &str) -> AppResult<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        )
    }

    fn document_symbols(&mut self, uri: &str) -> AppResult<Vec<SymbolNode>> {
        // Poll past the "still loading" error a freshly-started server returns
        // while indexing. The load loop is sequential, so the first file absorbs
        // the wait and the rest succeed immediately.
        let deadline = Instant::now() + SERVER_READY_TIMEOUT;
        loop {
            let outcome = self.request_with_timeout(
                "textDocument/documentSymbol",
                json!({
                    "textDocument": {
                        "uri": uri
                    }
                }),
                DOCUMENT_SYMBOLS_TIMEOUT,
            );
            match outcome {
                Ok(result) => return Ok(parse_symbols(&result)),
                Err(error) => {
                    let retryable = error
                        .downcast_ref::<LspError>()
                        .is_some_and(|e| e.retryable);
                    if retryable && Instant::now() < deadline {
                        thread::sleep(SERVER_READY_POLL_INTERVAL);
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> AppResult<Value> {
        self.request_inner(method, params, None)
    }

    fn request_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> AppResult<Value> {
        self.request_inner(method, params, Some(timeout))
    }

    fn request_inner(
        &mut self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> AppResult<Value> {
        let id = self.next_id;
        self.next_id += 1;

        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            let message = self.recv_message(deadline, method)?;

            if is_response_for(&message, id) {
                if let Some(error) = message.get("error") {
                    let code = error.get("code").and_then(Value::as_i64);
                    let detail = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(Box::new(LspError {
                        message: format!("LSP request `{method}` failed: {detail}"),
                        retryable: error_is_retryable(code, detail),
                    }));
                }

                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }

            if message.get("id").is_some() && message.get("method").is_some() {
                self.respond_to_server_request(&message)?;
            }
        }
    }

    fn recv_message(&self, deadline: Option<Instant>, method: &str) -> AppResult<Value> {
        match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(app_error(format!(
                        "LSP request `{method}` timed out after {:?}",
                        DOCUMENT_SYMBOLS_TIMEOUT
                    )));
                }
                match self.incoming_rx.recv_timeout(remaining) {
                    Ok(Ok(msg)) => Ok(msg),
                    Ok(Err(e)) => Err(e),
                    Err(RecvTimeoutError::Timeout) => Err(app_error(format!(
                        "LSP request `{method}` timed out after {:?}",
                        DOCUMENT_SYMBOLS_TIMEOUT
                    ))),
                    Err(RecvTimeoutError::Disconnected) => {
                        Err(app_error("LSP server closed connection"))
                    }
                }
            }
            None => match self.incoming_rx.recv() {
                Ok(Ok(msg)) => Ok(msg),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(app_error("LSP server closed connection")),
            },
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> AppResult<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn send(&mut self, message: Value) -> AppResult<()> {
        let body = serde_json::to_vec(&message).context("failed to serialize LSP message")?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())
            .context("failed to write LSP header")?;
        self.stdin
            .write_all(&body)
            .context("failed to write LSP body")?;
        self.stdin.flush().context("failed to flush LSP message")
    }

    fn respond_to_server_request(&mut self, request: &Value) -> AppResult<()> {
        let id = request
            .get("id")
            .cloned()
            .ok_or_else(|| app_error("LSP server request missing id"))?;
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

        let result = match method {
            "workspace/configuration" => {
                let item_count = request
                    .pointer("/params/items")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                Value::Array(vec![json!({}); item_count])
            }
            "workspace/workspaceFolders" => json!([{
                "uri": self.root_uri.clone(),
                "name": self.root_name.clone()
            }]),
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability" => Value::Null,
            "workspace/applyEdit" => json!({ "applied": false }),
            _ => Value::Null,
        };

        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }))
    }

    fn shutdown(&mut self) {
        // Bound the handshake: a misbehaving server that never replies to
        // `shutdown` or never honors `exit` must not be able to hang us. Both the
        // request and the wait are time-boxed, and `wait_or_kill` guarantees we
        // reach a `kill` — which closes the server's stdout and in turn unblocks
        // the reader thread if it is stuck mid-body in `read_exact`.
        let _ = self.request_with_timeout("shutdown", Value::Null, SHUTDOWN_REQUEST_TIMEOUT);
        let _ = self.notify("exit", Value::Null);
        wait_or_kill(&mut self.child, EXIT_WAIT_TIMEOUT);
    }
}

/// Wait up to `timeout` for the child to exit on its own; if it doesn't, kill it
/// and reap it. Never blocks longer than `timeout` plus the time for SIGKILL to
/// take effect. Killing the child closes its pipes, which releases the reader
/// thread if it is blocked on the process's stdout.
fn wait_or_kill(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Backstop for paths that don't call `shutdown` (e.g. an initialize
        // failure). SIGKILL can't be ignored, so the following wait returns
        // promptly; if `shutdown` already reaped the child these are no-ops.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_response_for(message: &Value, id: i64) -> bool {
    message
        .get("id")
        .and_then(Value::as_i64)
        .is_some_and(|message_id| message_id == id)
        && (message.get("result").is_some() || message.get("error").is_some())
}

fn parse_symbols(value: &Value) -> Vec<SymbolNode> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| {
            if item.get("location").is_some() {
                parse_symbol_information(item)
            } else {
                parse_document_symbol(item)
            }
        })
        .collect()
}

fn parse_document_symbol(value: &Value) -> Option<SymbolNode> {
    let name = value.get("name")?.as_str()?.to_string();
    let kind = value
        .get("kind")
        .and_then(Value::as_u64)
        .map(SymbolKind::from_lsp)
        .unwrap_or(SymbolKind::from_lsp(0));
    let line = value
        .pointer("/selectionRange/start/line")
        .or_else(|| value.pointer("/range/start/line"))
        .and_then(Value::as_u64)
        .map(|line| line as usize + 1);
    let detail = value
        .get("detail")
        .and_then(Value::as_str)
        .filter(|detail| !detail.is_empty())
        .map(ToOwned::to_owned);
    let children = value
        .get("children")
        .and_then(Value::as_array)
        .map(|children| children.iter().filter_map(parse_document_symbol).collect())
        .unwrap_or_default();

    Some(SymbolNode::new(name, kind, line, detail, children))
}

fn parse_symbol_information(value: &Value) -> Option<SymbolNode> {
    let name = value.get("name")?.as_str()?.to_string();
    let kind = value
        .get("kind")
        .and_then(Value::as_u64)
        .map(SymbolKind::from_lsp)
        .unwrap_or(SymbolKind::from_lsp(0));
    let line = value
        .pointer("/location/range/start/line")
        .and_then(Value::as_u64)
        .map(|line| line as usize + 1);
    let detail = value
        .get("containerName")
        .and_then(Value::as_str)
        .filter(|detail| !detail.is_empty())
        .map(|container| format!("in {container}"));

    Some(SymbolNode::new(name, kind, line, detail, Vec::new()))
}

fn read_lsp_message(reader: &mut BufReader<ChildStdout>) -> AppResult<Value> {
    let mut content_length = None;

    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .context("failed to read LSP header")?;
        if read == 0 {
            return Err(app_error("LSP server closed stdout"));
        }

        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }

        let Some((name, value)) = line.split_once(':') else {
            continue;
        };

        if name.eq_ignore_ascii_case("Content-Length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid LSP Content-Length")?,
            );
        }
    }

    let length = content_length.ok_or_else(|| app_error("LSP response missing Content-Length"))?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .context("failed to read LSP body")?;
    serde_json::from_slice(&body).context("failed to parse LSP JSON")
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn file_uri(path: &Path) -> String {
    let path = normalize_path(path);
    format!("file://{}", percent_encode(&path))
}

fn normalize_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) && !path.starts_with('/') {
        format!("/{path}")
    } else {
        path
    }
}

fn resolve_lsp_command(command: &str) -> PathBuf {
    if command.contains(std::path::MAIN_SEPARATOR) {
        return PathBuf::from(command);
    }
    let search_path = lsp_search_path();
    for dir in search_path.split(':').filter(|p| !p.is_empty()) {
        let candidate = Path::new(dir).join(command);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(command)
}

fn lsp_search_path() -> String {
    let extra = std::env::var("LSP_SEARCH_PATH").ok();
    let base = std::env::var("PATH").unwrap_or_default();
    match extra {
        Some(e) if !e.is_empty() && !base.is_empty() => format!("{e}:{base}"),
        Some(e) if !e.is_empty() => e,
        _ => base,
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let char = byte as char;
        if char.is_ascii_alphanumeric() || matches!(char, '-' | '_' | '.' | '~' | '/') {
            encoded.push(char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wait_or_kill_returns_for_process_that_exits_on_its_own() {
        // A process that exits immediately should be reaped well within the
        // timeout, with no kill needed.
        let mut child = Command::new("true").spawn().expect("spawn true");
        let start = Instant::now();
        wait_or_kill(&mut child, Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_secs(2), "should not block");
        // Already reaped: a second try_wait reports the cached exit status.
        assert!(matches!(child.try_wait(), Ok(Some(_))));
    }

    #[test]
    fn wait_or_kill_kills_a_process_that_never_exits() {
        // `sleep 600` stands in for a server that ignores `exit`. wait_or_kill
        // must give up after the timeout and force it down rather than hang.
        let mut child = Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("spawn sleep");
        let start = Instant::now();
        wait_or_kill(&mut child, Duration::from_millis(100));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "should have waited for the timeout"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "should not block far past the timeout (was {elapsed:?})"
        );
        // The process is gone and reaped.
        assert!(matches!(child.try_wait(), Ok(Some(_))));
    }

    #[test]
    fn parses_hierarchical_document_symbols() {
        let symbols = parse_symbols(&json!([
            {
                "name": "Parser",
                "kind": 23,
                "selectionRange": { "start": { "line": 4 } },
                "detail": "struct Parser",
                "children": [
                    {
                        "name": "parse",
                        "kind": 6,
                        "selectionRange": { "start": { "line": 8 } },
                        "detail": "fn(&self)"
                    }
                ]
            }
        ]));

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "Parser");
        assert_eq!(symbols[0].kind, SymbolKind::from_lsp(23));
        assert_eq!(symbols[0].line, Some(5));
        assert_eq!(symbols[0].detail.as_deref(), Some("struct Parser"));
        assert_eq!(symbols[0].children[0].name, "parse");
        assert_eq!(symbols[0].children[0].line, Some(9));
    }

    #[test]
    fn parses_flat_symbol_information() {
        let symbols = parse_symbols(&json!([
            {
                "name": "run",
                "kind": 12,
                "containerName": "app",
                "location": {
                    "uri": "file:///workspace/src/main.rs",
                    "range": { "start": { "line": 12 } }
                }
            }
        ]));

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "run");
        assert_eq!(symbols[0].kind, SymbolKind::from_lsp(12));
        assert_eq!(symbols[0].line, Some(13));
        assert_eq!(symbols[0].detail.as_deref(), Some("in app"));
    }

    #[test]
    fn encodes_file_uris() {
        assert_eq!(
            file_uri(Path::new("/tmp/rust project/src/main.rs")),
            "file:///tmp/rust%20project/src/main.rs"
        );
    }
}
