//! Built-in tools for Joi's agent harness.
//!
//! This crate owns concrete filesystem and process behavior. `joi-core` owns the registry,
//! permission state machine, and provider-neutral contracts.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use joi_core::tools::{
    Permission, PermissionAction, Tool, ToolCtx, ToolRegistry, ToolResult, ToolSchema,
};
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

/// The standard built-in tool set.
pub const STANDARD_BUILTINS: &[&str] = &["read", "list", "glob", "grep", "write", "edit", "bash"];

/// Build a registry from a configured list of names. Empty means the standard set.
pub fn builtins(names: &[String]) -> ToolRegistry {
    let selected: Vec<&str> = if names.is_empty() {
        STANDARD_BUILTINS.to_vec()
    } else {
        names.iter().map(String::as_str).collect()
    };
    let mut registry = ToolRegistry::new();
    for name in selected {
        match name {
            "read" => registry.register(Arc::new(ReadTool)),
            "list" => registry.register(Arc::new(ListTool)),
            "glob" => registry.register(Arc::new(GlobTool)),
            "grep" => registry.register(Arc::new(GrepTool)),
            "write" => registry.register(Arc::new(WriteTool)),
            "edit" => registry.register(Arc::new(EditTool)),
            "bash" => registry.register(Arc::new(BashTool)),
            other => tracing::warn!(tool = other, "unknown built-in tool configured"),
        }
    }
    registry
}

struct ReadTool;
struct ListTool;
struct GlobTool;
struct GrepTool;
struct WriteTool;
struct EditTool;
struct BashTool;

#[async_trait]
impl Tool for ReadTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "read",
            "Read a UTF-8 text file inside the readable workspace roots. Lines are prefixed with stable edit tags like 12:3af|; pass those tags to the edit tool.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to read."},
                    "max_bytes": {"type": "integer", "description": "Optional per-call byte cap."}
                },
                "required": ["path"]
            }),
        )
    }

    fn permission(&self, args: &Value, ctx: &ToolCtx) -> Permission {
        let subject = resolve_arg_path(args, ctx, "path");
        permission("read", subject, PermissionAction::Allow, "read file")
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(path) = arg_string(&args, "path") else {
            return ToolResult::error("missing path");
        };
        let path = resolve_path(ctx, &path);
        if !inside_any(&path, &ctx.readable_roots) {
            return ToolResult::error("path is outside readable roots");
        }
        let cap = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(ctx.max_output_bytes)
            .min(ctx.max_output_bytes);
        match tokio::fs::File::open(&path).await {
            Ok(file) => {
                let mut buf = Vec::new();
                let mut limited =
                    file.take(u64::try_from(cap.saturating_add(1)).unwrap_or(u64::MAX));
                if let Err(e) = limited.read_to_end(&mut buf).await {
                    return ToolResult::error(format!("read failed: {e}"));
                }
                let truncated = buf.len() > cap;
                buf.truncate(cap);
                ToolResult::ok(json!({
                    "path": display_path(&path),
                    "text": hashline_text(&String::from_utf8_lossy(&buf)),
                    "format": "line_hash_prefix",
                    "truncated": truncated,
                    "summary": if truncated { "read file (truncated)" } else { "read file" }
                }))
            }
            Err(e) => ToolResult::error(format!("open failed: {e}")),
        }
    }
}

#[async_trait]
impl Tool for ListTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "list",
            "List directory entries inside the readable workspace roots.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory path to list."},
                    "recursive": {"type": "boolean", "description": "Whether to walk recursively."}
                }
            }),
        )
    }

    fn permission(&self, args: &Value, ctx: &ToolCtx) -> Permission {
        let subject = resolve_arg_path_or_cwd(args, ctx, "path");
        permission("list", subject, PermissionAction::Allow, "list directory")
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let path =
            arg_string(&args, "path").map_or_else(|| ctx.cwd.clone(), |p| resolve_path(ctx, &p));
        if !inside_any(&path, &ctx.readable_roots) {
            return ToolResult::error("path is outside readable roots");
        }
        let recursive = args
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut entries = Vec::new();
        collect_entries(&path, recursive, ctx.max_output_bytes, &mut entries);
        ToolResult::ok(json!({
            "path": display_path(&path),
            "entries": entries,
            "summary": "listed directory"
        }))
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "glob",
            "Find paths matching a simple glob pattern inside the readable workspace roots.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Pattern supporting * and ?."},
                    "path": {"type": "string", "description": "Directory to search from."}
                },
                "required": ["pattern"]
            }),
        )
    }

    fn permission(&self, args: &Value, _ctx: &ToolCtx) -> Permission {
        let subject = arg_string(args, "pattern").unwrap_or_default();
        permission("glob", subject, PermissionAction::Allow, "search files")
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(pattern) = arg_string(&args, "pattern") else {
            return ToolResult::error("missing pattern");
        };
        let root =
            arg_string(&args, "path").map_or_else(|| ctx.cwd.clone(), |p| resolve_path(ctx, &p));
        if !inside_any(&root, &ctx.readable_roots) {
            return ToolResult::error("path is outside readable roots");
        }
        let mut matches = Vec::new();
        walk(&root, ctx.max_output_bytes, &mut |path| {
            if glob_match(&pattern, &display_path(path)) {
                matches.push(display_path(path));
            }
        });
        ToolResult::ok(json!({
            "matches": matches,
            "summary": "matched files"
        }))
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "grep",
            "Search text files for a literal pattern inside readable workspace roots.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Literal text to search for."},
                    "path": {"type": "string", "description": "File or directory to search."},
                    "case_sensitive": {"type": "boolean", "description": "Defaults to true."}
                },
                "required": ["pattern"]
            }),
        )
    }

    fn permission(&self, args: &Value, _ctx: &ToolCtx) -> Permission {
        let subject = arg_string(args, "pattern").unwrap_or_default();
        permission("grep", subject, PermissionAction::Allow, "search text")
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(pattern) = arg_string(&args, "pattern") else {
            return ToolResult::error("missing pattern");
        };
        let root =
            arg_string(&args, "path").map_or_else(|| ctx.cwd.clone(), |p| resolve_path(ctx, &p));
        if !inside_any(&root, &ctx.readable_roots) {
            return ToolResult::error("path is outside readable roots");
        }
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let needle = if case_sensitive {
            pattern.clone()
        } else {
            pattern.to_lowercase()
        };
        let mut matches = Vec::new();
        let mut search_file = |path: &Path| {
            if matches.len() >= 200 || is_probably_binary(path) {
                return;
            }
            let Ok(text) = std::fs::read_to_string(path) else {
                return;
            };
            for (idx, line) in text.lines().enumerate() {
                let hay = if case_sensitive {
                    line.to_string()
                } else {
                    line.to_lowercase()
                };
                if hay.contains(&needle) {
                    matches.push(json!({
                        "path": display_path(path),
                        "line": idx + 1,
                        "tag": format!("{}:{}", idx + 1, line_hash(line)),
                        "text": line
                    }));
                    if matches.len() >= 200 {
                        break;
                    }
                }
            }
        };
        if root.is_file() {
            search_file(&root);
        } else {
            walk(&root, ctx.max_output_bytes, &mut search_file);
        }
        ToolResult::ok(json!({
            "matches": matches,
            "summary": "searched text"
        }))
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "write",
            "Write a UTF-8 text file inside writable workspace roots.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to write."},
                    "content": {"type": "string", "description": "Full file content."}
                },
                "required": ["path", "content"]
            }),
        )
    }

    fn permission(&self, args: &Value, ctx: &ToolCtx) -> Permission {
        let subject = resolve_arg_path(args, ctx, "path");
        permission("edit", subject, PermissionAction::Ask, "write file")
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(path) = arg_string(&args, "path") else {
            return ToolResult::error("missing path");
        };
        let Some(content) = arg_string(&args, "content") else {
            return ToolResult::error("missing content");
        };
        let path = resolve_path(ctx, &path);
        if !inside_any(&path, &ctx.writable_roots) {
            return ToolResult::error("path is outside writable roots");
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::error(format!("create parent failed: {e}"));
            }
        }
        match tokio::fs::write(&path, content).await {
            Ok(()) => ToolResult::ok(json!({
                "path": display_path(&path),
                "summary": "wrote file"
            })),
            Err(e) => ToolResult::error(format!("write failed: {e}")),
        }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "edit",
            "Edit a UTF-8 file using line hash tags returned by read. Replace one line with start, a range with start/end, or insert after a tag with after. Legacy old/new replacement is still accepted.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to edit."},
                    "start": {"type": "string", "description": "Start line tag from read, e.g. 12:3af. Replaces this line when end is omitted."},
                    "end": {"type": "string", "description": "Optional inclusive end line tag from read for range replacement."},
                    "after": {"type": "string", "description": "Line tag from read. Inserts new text after this line."},
                    "new": {"type": "string", "description": "Replacement or insertion text without hash prefixes."},
                    "old": {"type": "string", "description": "Legacy exact text to replace when hash tags are not supplied."},
                    "replace_all": {"type": "boolean", "description": "Replace all matches instead of the first."}
                },
                "required": ["path", "new"]
            }),
        )
    }

    fn permission(&self, args: &Value, ctx: &ToolCtx) -> Permission {
        let subject = resolve_arg_path(args, ctx, "path");
        permission("edit", subject, PermissionAction::Ask, "edit file")
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(path) = arg_string(&args, "path") else {
            return ToolResult::error("missing path");
        };
        let Some(new) = arg_string(&args, "new") else {
            return ToolResult::error("missing new");
        };
        let path = resolve_path(ctx, &path);
        if !inside_any(&path, &ctx.writable_roots) || !inside_any(&path, &ctx.readable_roots) {
            return ToolResult::error("path is outside editable roots");
        }
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(e) => return ToolResult::error(format!("read failed: {e}")),
        };
        let updated = if let Some(after) = arg_string(&args, "after") {
            match insert_after_tag(&text, &after, &new) {
                Ok(updated) => updated,
                Err(e) => return ToolResult::error(e),
            }
        } else if let Some(start) = arg_string(&args, "start") {
            let end = arg_string(&args, "end").unwrap_or_else(|| start.clone());
            match replace_tag_range(&text, &start, &end, &new) {
                Ok(updated) => updated,
                Err(e) => return ToolResult::error(e),
            }
        } else if let Some(old) = arg_string(&args, "old") {
            if !text.contains(&old) {
                return ToolResult::error("old text was not found");
            }
            let replace_all = args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if replace_all {
                text.replace(&old, &new)
            } else {
                text.replacen(&old, &new, 1)
            }
        } else {
            return ToolResult::error("provide start/end, after, or legacy old text");
        };
        match tokio::fs::write(&path, updated).await {
            Ok(()) => ToolResult::ok(json!({
                "path": display_path(&path),
                "summary": "edited file"
            })),
            Err(e) => ToolResult::error(format!("write failed: {e}")),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn schema(&self) -> ToolSchema {
        schema(
            "bash",
            "Run a non-interactive bash command in the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Command to run with bash -lc."}
                },
                "required": ["command"]
            }),
        )
    }

    fn permission(&self, args: &Value, ctx: &ToolCtx) -> Permission {
        let command = arg_string(args, "command").unwrap_or_default();
        let action = if !ctx.network && looks_like_network_command(&command) {
            PermissionAction::Deny
        } else if looks_read_only(&command) {
            PermissionAction::Allow
        } else {
            PermissionAction::Ask
        };
        permission("bash", command, action, "run bash command")
    }

    async fn run(&self, args: Value, ctx: &ToolCtx) -> ToolResult {
        let Some(command) = arg_string(&args, "command") else {
            return ToolResult::error("missing command");
        };
        if !ctx.network && looks_like_network_command(&command) {
            return ToolResult::error("network commands are disabled");
        }
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-lc")
            .arg(&command)
            .current_dir(&ctx.cwd)
            .env("NO_COLOR", "1")
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return ToolResult::error(format!("spawn failed: {e}")),
        };
        let output = child.wait_with_output().await;
        match output {
            Ok(output) => {
                let stdout = cap_text(&output.stdout, ctx.max_output_bytes);
                let stderr = cap_text(&output.stderr, ctx.max_output_bytes);
                let code = output.status.code();
                ToolResult::ok(json!({
                    "exit_code": code,
                    "stdout": stdout,
                    "stderr": stderr,
                    "summary": match code {
                        Some(0) => "bash completed",
                        _ => "bash exited non-zero"
                    }
                }))
            }
            Err(e) => ToolResult::error(format!("command failed: {e}")),
        }
    }
}

fn schema(name: &str, description: &str, parameters: Value) -> ToolSchema {
    ToolSchema {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

fn permission(key: &str, subject: String, action: PermissionAction, summary: &str) -> Permission {
    Permission {
        key: key.to_string(),
        subject: subject.clone(),
        default_action: action,
        summary: format!("{summary}: {subject}"),
        detail: subject,
    }
}

fn arg_string(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn resolve_arg_path(args: &Value, ctx: &ToolCtx, key: &str) -> String {
    arg_string(args, key).map_or_else(String::new, |path| display_path(resolve_path(ctx, &path)))
}

fn resolve_arg_path_or_cwd(args: &Value, ctx: &ToolCtx, key: &str) -> String {
    arg_string(args, key).map_or_else(
        || display_path(&ctx.cwd),
        |path| display_path(resolve_path(ctx, &path)),
    )
}

fn resolve_path(ctx: &ToolCtx, path: &str) -> PathBuf {
    normalize_path(
        if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            ctx.cwd.join(path)
        }
        .as_path(),
    )
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn inside_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

fn display_path(path: impl AsRef<Path>) -> String {
    path.as_ref().display().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TextLine {
    body: String,
    eol: String,
}

fn hashline_text(text: &str) -> String {
    split_lines_preserving(text).into_iter().enumerate().fold(
        String::new(),
        |mut out, (idx, line)| {
            let _ = write!(
                out,
                "{}:{}|{}{}",
                idx + 1,
                line_hash(&line.body),
                line.body,
                line.eol
            );
            out
        },
    )
}

fn replace_tag_range(text: &str, start: &str, end: &str, new: &str) -> Result<String, String> {
    let lines = split_lines_preserving(text);
    let start_idx = locate_tag(&lines, start)?;
    let end_idx = locate_tag(&lines, end)?;
    if end_idx < start_idx {
        return Err("end tag comes before start tag".to_string());
    }
    let mut out = String::new();
    for line in &lines[..start_idx] {
        out.push_str(&line.body);
        out.push_str(&line.eol);
    }
    let trailing_eol = lines
        .get(end_idx)
        .map_or_else(|| file_eol(&lines), |line| line.eol.clone());
    out.push_str(&line_edit_block(new, &trailing_eol));
    for line in &lines[end_idx + 1..] {
        out.push_str(&line.body);
        out.push_str(&line.eol);
    }
    Ok(out)
}

fn insert_after_tag(text: &str, after: &str, new: &str) -> Result<String, String> {
    let mut lines = split_lines_preserving(text);
    let after_idx = locate_tag(&lines, after)?;
    let eol = file_eol(&lines);
    let mut out = String::new();
    for (idx, line) in lines.iter_mut().enumerate() {
        out.push_str(&line.body);
        if idx == after_idx && line.eol.is_empty() {
            out.push_str(&eol);
        } else {
            out.push_str(&line.eol);
        }
        if idx == after_idx {
            out.push_str(&line_edit_block(new, &eol));
        }
    }
    Ok(out)
}

fn locate_tag(lines: &[TextLine], tag: &str) -> Result<usize, String> {
    let Some((line_no, hash)) = parse_line_tag(tag) else {
        return Err(format!(
            "invalid line tag `{tag}`; expected line:hash, e.g. 12:3af"
        ));
    };
    let Some(idx) = line_no.checked_sub(1) else {
        return Err(format!("invalid line tag `{tag}`; line numbers start at 1"));
    };
    let Some(line) = lines.get(idx) else {
        return Err(format!(
            "line tag `{tag}` is outside the current file; read it again"
        ));
    };
    let current = line_hash(&line.body);
    if current != hash {
        return Err(format!(
            "line tag `{tag}` no longer matches current line {line_no} (now {current}); read the file again"
        ));
    }
    Ok(idx)
}

fn parse_line_tag(tag: &str) -> Option<(usize, String)> {
    let (line, hash) = tag.split_once(':')?;
    if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let line = line.parse::<usize>().ok()?;
    Some((line, hash.to_ascii_lowercase()))
}

fn split_lines_preserving(text: &str) -> Vec<TextLine> {
    let mut lines = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0usize;
    for (idx, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        let (body_end, eol) = if idx > start && bytes[idx - 1] == b'\r' {
            (idx - 1, "\r\n")
        } else {
            (idx, "\n")
        };
        lines.push(TextLine {
            body: text[start..body_end].to_string(),
            eol: eol.to_string(),
        });
        start = idx + 1;
    }
    if start < text.len() {
        lines.push(TextLine {
            body: text[start..].to_string(),
            eol: String::new(),
        });
    }
    lines
}

fn line_edit_block(new: &str, trailing_eol: &str) -> String {
    if new.is_empty() {
        return String::new();
    }
    let normalized = normalize_newlines(new, trailing_eol);
    if trailing_eol.is_empty() || normalized.ends_with(trailing_eol) {
        normalized
    } else {
        format!("{normalized}{trailing_eol}")
    }
}

fn normalize_newlines(text: &str, eol: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if eol == "\n" || eol.is_empty() {
        normalized
    } else {
        normalized.replace('\n', eol)
    }
}

fn file_eol(lines: &[TextLine]) -> String {
    lines
        .iter()
        .find(|line| !line.eol.is_empty())
        .map_or_else(|| "\n".to_string(), |line| line.eol.clone())
}

fn line_hash(line: &str) -> String {
    // FNV-1a, truncated for compact line tags. The line number is part of the tag, so the hash is
    // only a stale-content guard, not a global identifier.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in line.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:03x}", hash & 0x0fff)
}

fn collect_entries(path: &Path, recursive: bool, cap: usize, entries: &mut Vec<Value>) {
    let Ok(read_dir) = std::fs::read_dir(path) else {
        return;
    };
    for entry in read_dir.flatten() {
        if entries.len() >= cap / 64 {
            break;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        entries.push(json!({
            "path": display_path(&path),
            "kind": if is_dir { "dir" } else { "file" }
        }));
        if recursive && is_dir {
            collect_entries(&path, recursive, cap, entries);
        }
    }
}

fn walk(root: &Path, cap: usize, f: &mut impl FnMut(&Path)) {
    fn inner(path: &Path, seen: &mut usize, max: usize, f: &mut impl FnMut(&Path)) {
        if *seen >= max {
            return;
        }
        *seen += 1;
        f(path);
        let Ok(read_dir) = std::fs::read_dir(path) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.is_dir() {
                inner(&path, seen, max, f);
            } else {
                *seen += 1;
                f(&path);
            }
            if *seen >= max {
                break;
            }
        }
    }
    let mut seen = 0;
    let max = (cap / 64).clamp(100, 10_000);
    inner(root, &mut seen, max, f);
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (Some(b'*'), _) => go(&p[1..], t) || (!t.is_empty() && go(p, &t[1..])),
            (Some(b'?'), Some(_)) => go(&p[1..], &t[1..]),
            (Some(a), Some(b)) if a == b => go(&p[1..], &t[1..]),
            _ => false,
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

fn is_probably_binary(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "pdf" | "zip" | "gz" | "xz" | "mp3" | "mp4")
    )
}

fn cap_text(bytes: &[u8], cap: usize) -> String {
    let mut slice = bytes;
    if slice.len() > cap {
        slice = &slice[..cap];
    }
    String::from_utf8_lossy(slice).to_string()
}

fn looks_like_network_command(command: &str) -> bool {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))
        .any(|part| {
            matches!(
                part,
                "curl" | "wget" | "nc" | "ncat" | "ssh" | "scp" | "rsync"
            )
        })
}

fn looks_read_only(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or_default();
    matches!(
        first,
        "cat" | "ls" | "pwd" | "rg" | "grep" | "find" | "sed" | "head" | "tail" | "wc" | "git"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use joi_core::clock::TestClock;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio_util::sync::CancellationToken;

    fn temp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("joi-tools-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn ctx(root: PathBuf) -> ToolCtx {
        ToolCtx {
            readable_roots: vec![root.clone()],
            writable_roots: vec![root.clone()],
            cwd: root,
            timeout: Duration::from_secs(1),
            max_output_bytes: 4096,
            network: false,
            cancel: CancellationToken::new(),
            clock: Arc::new(TestClock::new(1_000)),
        }
    }

    #[tokio::test]
    async fn read_rejects_paths_outside_readable_roots() {
        let root = temp_root();
        let ctx = ctx(root);
        let registry = builtins(&["read".to_string()]);
        let tool = registry.get("read").unwrap();
        let result = tool.run(json!({"path": "/etc/passwd"}), &ctx).await;
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn read_prefixes_lines_with_hash_tags() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\n").unwrap();
        let ctx = ctx(root);
        let registry = builtins(&["read".to_string()]);
        let tool = registry.get("read").unwrap();
        let result = tool.run(json!({"path": "a.txt"}), &ctx).await;
        assert!(result.ok);
        let text = result.content["text"].as_str().unwrap();
        assert!(text.contains(&format!("1:{}|alpha", line_hash("alpha"))));
        assert!(text.contains(&format!("2:{}|beta", line_hash("beta"))));
        assert_eq!(result.content["format"], "line_hash_prefix");
    }

    #[tokio::test]
    async fn write_creates_file_inside_writable_roots() {
        let root = temp_root();
        let ctx = ctx(root.clone());
        let registry = builtins(&["write".to_string()]);
        let tool = registry.get("write").unwrap();
        assert_eq!(
            tool.permission(&json!({"path": "a.txt", "content": "hi"}), &ctx)
                .default_action,
            PermissionAction::Ask
        );
        let result = tool
            .run(json!({"path": "a.txt", "content": "hi"}), &ctx)
            .await;
        assert!(result.ok);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hi");
    }

    #[tokio::test]
    async fn edit_replaces_hash_addressed_line() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ctx = ctx(root.clone());
        let registry = builtins(&["edit".to_string()]);
        let tool = registry.get("edit").unwrap();
        let tag = format!("2:{}", line_hash("beta"));
        let result = tool
            .run(json!({"path": "a.txt", "start": tag, "new": "BETA"}), &ctx)
            .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_replaces_hash_addressed_range() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ctx = ctx(root.clone());
        let registry = builtins(&["edit".to_string()]);
        let tool = registry.get("edit").unwrap();
        let start = format!("2:{}", line_hash("beta"));
        let end = format!("3:{}", line_hash("gamma"));
        let result = tool
            .run(
                json!({"path": "a.txt", "start": start, "end": end, "new": "delta\nepsilon"}),
                &ctx,
            )
            .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "alpha\ndelta\nepsilon\n"
        );
    }

    #[tokio::test]
    async fn edit_inserts_after_hash_addressed_line() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "alpha\nbeta").unwrap();
        let ctx = ctx(root.clone());
        let registry = builtins(&["edit".to_string()]);
        let tool = registry.get("edit").unwrap();
        let after = format!("2:{}", line_hash("beta"));
        let result = tool
            .run(
                json!({"path": "a.txt", "after": after, "new": "gamma"}),
                &ctx,
            )
            .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "alpha\nbeta\ngamma\n"
        );
    }

    #[tokio::test]
    async fn edit_rejects_stale_hash_tag() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\n").unwrap();
        let ctx = ctx(root);
        let registry = builtins(&["edit".to_string()]);
        let tool = registry.get("edit").unwrap();
        let result = tool
            .run(
                json!({"path": "a.txt", "start": "2:000", "new": "BETA"}),
                &ctx,
            )
            .await;
        assert!(!result.ok);
        assert!(result.content["error"]
            .as_str()
            .unwrap()
            .contains("no longer matches"));
    }

    #[tokio::test]
    async fn edit_legacy_string_replace_still_works() {
        let root = temp_root();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\n").unwrap();
        let ctx = ctx(root.clone());
        let registry = builtins(&["edit".to_string()]);
        let tool = registry.get("edit").unwrap();
        let result = tool
            .run(json!({"path": "a.txt", "old": "beta", "new": "BETA"}), &ctx)
            .await;
        assert!(result.ok, "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "alpha\nBETA\n"
        );
    }

    #[test]
    fn bash_network_command_is_denied_by_default() {
        let root = temp_root();
        let ctx = ctx(root);
        let registry = builtins(&["bash".to_string()]);
        let tool = registry.get("bash").unwrap();
        let permission = tool.permission(&json!({"command": "curl https://example.com"}), &ctx);
        assert_eq!(permission.default_action, PermissionAction::Deny);
    }
}
