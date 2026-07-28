//! Filesystem-backed scoped memory under `~/.config/weave/memory/`.
//!
//! Plain markdown files with YAML frontmatter; simple substring search;
//! no SQLite/FTS, no async.  All I/O is synchronous std::fs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Constants — input caps
// ---------------------------------------------------------------------------

const MAX_KEY_LEN: usize = 128;
const MAX_TITLE_LEN: usize = 256;
const MAX_TAGS: usize = 16;
const MAX_TAG_LEN: usize = 64;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_FILES_PER_SCOPE: usize = 10_000;
const MAX_SEARCH_RESULTS: usize = 50;
const MAX_CONTEXT_PREFIX_ENTRIES: usize = 5;

const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "must", "shall",
    "can", "need", "dare", "ought", "used", "to", "of", "in", "for", "on", "with", "at", "by",
    "from", "as", "into", "through", "during", "before", "after", "above", "below", "between",
    "under", "and", "but", "or", "yet", "so", "if", "because", "although", "though", "while",
    "where", "when", "that", "which", "who", "whom", "whose", "what", "this", "these", "those",
    "i", "you", "he", "she", "it", "we", "they", "me", "him", "her", "us", "them",
];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemoryScope {
    Global,
    Project(String),
    Persona(String),
    Orchestrator(String),
}

impl MemoryScope {
    /// Human-readable scope label used in display paths.
    pub fn label(&self) -> String {
        match self {
            MemoryScope::Global => "global".to_string(),
            MemoryScope::Project(p) => format!("project::{p}"),
            MemoryScope::Persona(p) => format!("persona::{p}"),
            MemoryScope::Orchestrator(c) => format!("orchestrator::{c}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub scope: MemoryScope,
    pub key: String,
    pub title: String,
    pub tags: Vec<String>,
    pub created_ts: i64,
    pub updated_ts: i64,
    pub body: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Return the absolute directory for a scope.
pub fn memory_dir(scope: &MemoryScope) -> PathBuf {
    let base = config_memory_dir();
    match scope {
        MemoryScope::Global => base.join("global"),
        MemoryScope::Project(p) => base.join("project").join(sanitize_dir_component(p)),
        MemoryScope::Persona(p) => base.join("persona").join(sanitize_dir_component(p)),
        MemoryScope::Orchestrator(c) => base.join("orchestrator").join(sanitize_dir_component(c)),
    }
}

/// Return the absolute path for a scope + key.
pub fn memory_path(scope: &MemoryScope, key: &str) -> anyhow::Result<PathBuf> {
    let key = sanitize_key(key)?;
    Ok(memory_dir(scope).join(format!("{key}.md")))
}

/// Write or overwrite a memory entry. Creates parent dirs.
pub fn memory_write(
    scope: &MemoryScope,
    key: &str,
    title: &str,
    tags: &[String],
    body: &str,
) -> anyhow::Result<()> {
    let key = sanitize_key(key)?;
    let title = sanitize_title(title);
    let tags = sanitize_tags(tags);

    if body.len() > MAX_BODY_BYTES {
        anyhow::bail!(
            "body is too long ({} bytes; max {})",
            body.len(),
            MAX_BODY_BYTES
        );
    }

    let dir = memory_dir(scope);
    std::fs::create_dir_all(&dir)?;

    // Cap file count per scope.
    let count = std::fs::read_dir(&dir)?.count();
    if count >= MAX_FILES_PER_SCOPE {
        anyhow::bail!(
            "scope file count cap reached ({MAX_FILES_PER_SCOPE}); delete an entry first"
        );
    }

    let path = dir.join(format!("{key}.md"));
    let now = crate::model::now();

    // Preserve created_ts if the file already exists.
    let created_ts = if path.exists() {
        parse_entry(&path).map(|e| e.created_ts).unwrap_or(now)
    } else {
        now
    };

    let entry = MemoryEntry {
        scope: scope.clone(),
        key: key.clone(),
        title,
        tags,
        created_ts,
        updated_ts: now,
        body: body.to_string(),
    };

    let text = format_entry(&entry);
    std::fs::write(&path, text)?;
    Ok(())
}

/// Read a memory entry by scope + key.
pub fn memory_read(scope: &MemoryScope, key: &str) -> anyhow::Result<MemoryEntry> {
    let key = sanitize_key(key)?;
    let path = memory_dir(scope).join(format!("{key}.md"));
    if !path.exists() {
        anyhow::bail!("memory entry not found: {}/{key}", scope.label());
    }
    let mut entry = parse_entry(&path)?;
    entry.scope = scope.clone();
    entry.key = key;
    Ok(entry)
}

/// Search memory entries in a scope (or all scopes if `scope = None`).
/// Simple substring grep over the full file contents (frontmatter + body).
/// Returns matches sorted by relevance (exact tag match > substring in title/body).
pub fn memory_search(scope: Option<&MemoryScope>, query: &str) -> anyhow::Result<Vec<MemoryEntry>> {
    let query_lc = query.to_lowercase();
    let mut hits: Vec<(u32, MemoryEntry)> = Vec::new();

    let scopes: Vec<MemoryScope> = match scope {
        Some(s) => vec![s.clone()],
        None => memory_scopes()?, // only scopes with >=1 entry
    };

    for s in scopes {
        let dir = memory_dir(&s);
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.extension().map(|e| e == "md").unwrap_or(false) {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            let score = relevance_score(&query_lc, &raw);
            if score > 0 {
                if let Ok(mut e) = parse_entry(&path) {
                    e.scope = s.clone();
                    e.key = path
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    hits.push((score, e));
                }
            }
        }
    }

    hits.sort_by_key(|b| std::cmp::Reverse(b.0)); // descending score
    hits.truncate(MAX_SEARCH_RESULTS);
    Ok(hits.into_iter().map(|(_, e)| e).collect())
}

/// List all entries in a scope.
pub fn memory_list(scope: &MemoryScope) -> anyhow::Result<Vec<MemoryEntry>> {
    let dir = memory_dir(scope);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.extension().map(|e| e == "md").unwrap_or(false) {
            continue;
        }
        if let Ok(mut e) = parse_entry(&path) {
            e.scope = scope.clone();
            e.key = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            out.push(e);
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// Delete an entry. Returns true if it existed and was removed.
pub fn memory_delete(scope: &MemoryScope, key: &str) -> anyhow::Result<bool> {
    let key = sanitize_key(key)?;
    let path = memory_dir(scope).join(format!("{key}.md"));
    if path.exists() {
        std::fs::remove_file(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Return all available scopes that currently have at least one entry on disk.
pub fn memory_scopes() -> anyhow::Result<Vec<MemoryScope>> {
    let base = config_memory_dir();
    let mut out = Vec::new();

    // global
    let global = base.join("global");
    if has_md_file(&global) {
        out.push(MemoryScope::Global);
    }

    // project/<name>/
    let project = base.join("project");
    if project.exists() {
        for entry in std::fs::read_dir(&project)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && has_md_file(&entry.path()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                out.push(MemoryScope::Project(name));
            }
        }
    }

    // persona/<name>/
    let persona = base.join("persona");
    if persona.exists() {
        for entry in std::fs::read_dir(&persona)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && has_md_file(&entry.path()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                out.push(MemoryScope::Persona(name));
            }
        }
    }

    // orchestrator/<name>/
    let orchestrator = base.join("orchestrator");
    if orchestrator.exists() {
        for entry in std::fs::read_dir(&orchestrator)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && has_md_file(&entry.path()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                out.push(MemoryScope::Orchestrator(name));
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Scope resolution helpers
// ---------------------------------------------------------------------------

/// Resolve the current project scope from cwd (best-effort; returns None if not in a git repo).
pub fn project_scope_from_cwd() -> Option<MemoryScope> {
    let cwd = std::env::current_dir().ok()?;
    let dot_git = cwd.join(".git");
    if dot_git.exists() {
        let repo = cwd
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        if !repo.is_empty() {
            Some(MemoryScope::Project(repo))
        } else {
            None
        }
    } else {
        None
    }
}

/// Resolve the current persona scope from an identity string.
pub fn persona_scope(identity: &str) -> MemoryScope {
    MemoryScope::Persona(identity.to_string())
}

/// Resolve the current orchestrator scope from a circle string.
pub fn orchestrator_scope(circle: &str) -> MemoryScope {
    MemoryScope::Orchestrator(circle.to_string())
}

// ---------------------------------------------------------------------------
// Context prefixing
// ---------------------------------------------------------------------------

/// Build a memory context prefix for a given body + current scopes.
/// Searches all resolved scopes (global, project from cwd, persona from identity,
/// orchestrator from circle) for entries whose tags/body match keywords extracted
/// from the body.  Non-fatal: on any error returns an empty string.
pub fn build_context_prefix(identity: &str, circle: &str, body: &str, top_n: usize) -> String {
    let top_n = top_n.min(MAX_CONTEXT_PREFIX_ENTRIES);
    let keywords = extract_keywords(body);
    if keywords.is_empty() {
        return String::new();
    }

    let mut scopes: Vec<MemoryScope> = vec![MemoryScope::Global];
    if let Some(ps) = project_scope_from_cwd() {
        scopes.push(ps);
    }
    scopes.push(MemoryScope::Persona(identity.to_string()));
    scopes.push(MemoryScope::Orchestrator(circle.to_string()));

    let mut seen: HashSet<String> = HashSet::new();
    let mut scored: Vec<(u32, MemoryEntry)> = Vec::new();

    for kw in &keywords {
        for s in &scopes {
            let dir = memory_dir(s);
            if !dir.exists() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let path = entry.path();
                if !path.extension().map(|e| e == "md").unwrap_or(false) {
                    continue;
                }
                let raw = std::fs::read_to_string(&path).unwrap_or_default();
                let score = relevance_score_kw(kw, &raw);
                if score == 0 {
                    continue;
                }
                let key = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let uniq = format!("{}::{}::{}", s.label(), key, score);
                if seen.contains(&uniq) {
                    continue;
                }
                seen.insert(uniq);
                if let Ok(mut e) = parse_entry(&path) {
                    e.scope = s.clone();
                    e.key = key;
                    scored.push((score, e));
                }
            }
        }
    }

    scored.sort_by_key(|b| std::cmp::Reverse(b.0));
    scored.truncate(top_n);

    if scored.is_empty() {
        return String::new();
    }

    let mut out = String::from("<weave-memory>\n");
    for (_, e) in scored {
        let scope_label = e.scope.label();
        let preview: String = e.body.lines().next().unwrap_or("").to_string();
        out.push_str(&format!("- [{scope_label}::{}] {}\n", e.key, e.title));
        if !preview.is_empty() {
            out.push_str(&format!("  {preview}\n"));
        }
    }
    out.push_str("</weave-memory>\n\n<original body follows>\n\n");
    out
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn config_memory_dir() -> PathBuf {
    crate::config::config_dir().join("memory")
}

fn sanitize_key(key: &str) -> anyhow::Result<String> {
    if key.is_empty() {
        anyhow::bail!("key must not be empty");
    }
    if key.len() > MAX_KEY_LEN {
        anyhow::bail!("key is too long (max {MAX_KEY_LEN} chars)");
    }
    if key.contains("..") || key.contains('/') || key.contains('\\') {
        anyhow::bail!("key contains path traversal characters");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("key must be [a-zA-Z0-9_-] only");
    }
    Ok(key.to_string())
}

fn sanitize_title(title: &str) -> String {
    let t = title.trim();
    if t.chars().count() > MAX_TITLE_LEN {
        t.chars().take(MAX_TITLE_LEN).collect()
    } else {
        t.to_string()
    }
}

fn sanitize_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags.iter().take(MAX_TAGS) {
        let s = t
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .take(MAX_TAG_LEN)
            .collect::<String>();
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

fn sanitize_dir_component(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn has_md_file(dir: &Path) -> bool {
    if !dir.exists() {
        return false;
    }
    std::fs::read_dir(dir)
        .ok()
        .and_then(|mut rd| {
            rd.find(|e| {
                e.as_ref()
                    .map(|e| e.path().extension().map(|ex| ex == "md").unwrap_or(false))
                    .unwrap_or(false)
            })
        })
        .is_some()
}

fn parse_entry(path: &Path) -> anyhow::Result<MemoryEntry> {
    let text = std::fs::read_to_string(path)?;

    // Must start with frontmatter delimiter.
    let (fm, body) = split_frontmatter(&text)
        .ok_or_else(|| anyhow::anyhow!("memory file missing or malformed YAML frontmatter"))?;

    let mut title = String::new();
    let mut tags: Vec<String> = Vec::new();
    let mut created_ts: i64 = 0;
    let mut updated_ts: i64 = 0;

    for line in fm.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        match k {
            "title" => {
                title = v.trim_matches('"').trim_matches('\'').to_string();
            }
            "tags" => {
                tags = parse_tag_array(v);
            }
            "created_ts" => {
                created_ts = v.parse().unwrap_or(0);
            }
            "updated_ts" => {
                updated_ts = v.parse().unwrap_or(0);
            }
            _ => {}
        }
    }

    Ok(MemoryEntry {
        scope: MemoryScope::Global, // caller overwrites
        key: String::new(),         // caller overwrites
        title,
        tags,
        created_ts,
        updated_ts,
        body: body.to_string(),
    })
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    // Accept "---\n" or "---\r\n"
    let text = text.strip_prefix("---")?;
    let text = text
        .strip_prefix("\r\n")
        .or_else(|| text.strip_prefix('\n'))?;
    let end = text.find("\n---")?;
    let fm = &text[..end];
    let body_start = end + 4; // past "\n---"
    let body = text[body_start..]
        .trim_start_matches('\r')
        .trim_start_matches('\n');
    Some((fm, body))
}

fn parse_tag_array(s: &str) -> Vec<String> {
    let s = s.trim();
    if !s.starts_with('[') || !s.ends_with(']') {
        return Vec::new();
    }
    let inner = &s[1..s.len() - 1];
    inner
        .split(',')
        .map(|t| t.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn format_entry(entry: &MemoryEntry) -> String {
    let tags_json = if entry.tags.is_empty() {
        "[]".to_string()
    } else {
        let items: Vec<String> = entry.tags.iter().map(|t| format!("\"{t}\"")).collect();
        format!("[{}]", items.join(", "))
    };
    format!(
        "---\ntitle: \"{}\"\ntags: {tags_json}\ncreated_ts: {}\nupdated_ts: {}\n---\n\n{}",
        entry.title.replace('"', "\\\""),
        entry.created_ts,
        entry.updated_ts,
        entry.body
    )
}

fn relevance_score(query_lc: &str, raw: &str) -> u32 {
    let raw_lc = raw.to_lowercase();
    if raw_lc.contains(query_lc) {
        // Tag match is worth more than body match.
        if raw_lc.contains(&format!("tags: [{query_lc}]"))
            || raw_lc.contains(&format!("\"{query_lc}\""))
        {
            100
        } else {
            10
        }
    } else {
        0
    }
}

fn relevance_score_kw(kw_lc: &str, raw: &str) -> u32 {
    relevance_score(kw_lc, raw)
}

fn extract_keywords(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in body.split_whitespace() {
        let w = word
            .to_lowercase()
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if w.len() < 2 {
            continue;
        }
        if STOP_WORDS.contains(&w.as_str()) {
            continue;
        }
        if !out.contains(&w) {
            out.push(w);
        }
        if out.len() >= 10 {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static FS_LOCK: Mutex<()> = Mutex::new(());
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_memory_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "weave-memory-test-{}-{}-{}",
            std::process::id(),
            crate::model::now(),
            n
        ));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        dir.join("weave").join("memory")
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sanitize_key_accepts_good_rejects_bad() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(sanitize_key("foo-bar_123").unwrap(), "foo-bar_123");
        assert!(sanitize_key("../etc").is_err());
        assert!(sanitize_key("foo/bar").is_err());
        assert!(sanitize_key("foo\\bar").is_err());
        assert!(sanitize_key("").is_err());
        assert!(sanitize_key(&"x".repeat(MAX_KEY_LEN + 1)).is_err());
        assert!(sanitize_key("foo;rm").is_err());
    }

    #[test]
    fn memory_roundtrip() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        memory_write(
            &scope,
            "roundtrip",
            "My Title",
            &["rust".into(), "test".into()],
            "Body here.",
        )
        .unwrap();
        let e = memory_read(&scope, "roundtrip").unwrap();
        assert_eq!(e.title, "My Title");
        assert_eq!(e.tags, vec!["rust", "test"]);
        assert_eq!(e.body, "Body here.");
        cleanup(&base);
    }

    #[test]
    fn memory_search_substring() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        memory_write(&scope, "a", "A", &[], "alpha content").unwrap();
        memory_write(&scope, "b", "B", &[], "beta content").unwrap();
        let hits = memory_search(Some(&scope), "alpha").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "a");
        cleanup(&base);
    }

    #[test]
    fn memory_search_tag_priority() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        memory_write(&scope, "only-body", "X", &[], "rusty body").unwrap();
        memory_write(&scope, "has-tag", "Y", &["rusty".into()], "other body").unwrap();
        let hits = memory_search(Some(&scope), "rusty").unwrap();
        assert_eq!(hits.len(), 2);
        // Tag match should rank higher.
        assert_eq!(hits[0].key, "has-tag");
        cleanup(&base);
    }

    #[test]
    fn memory_list_and_delete() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        memory_write(&scope, "x", "X", &[], "body").unwrap();
        memory_write(&scope, "y", "Y", &[], "body").unwrap();
        let list = memory_list(&scope).unwrap();
        assert_eq!(list.len(), 2);
        assert!(memory_delete(&scope, "x").unwrap());
        let list = memory_list(&scope).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key, "y");
        cleanup(&base);
    }

    #[test]
    fn memory_path_is_under_config_dir() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/weave-cfg-test");
        let p = memory_path(&MemoryScope::Global, "foo").unwrap();
        assert!(p.starts_with("/tmp/weave-cfg-test/weave/memory/"));
        let p = memory_path(&MemoryScope::Project("weave".into()), "bar").unwrap();
        assert!(p.starts_with("/tmp/weave-cfg-test/weave/memory/project/"));
    }

    #[test]
    fn parse_entry_handles_empty_body() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("weave-mem-parse-{}", crate::model::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.md");
        std::fs::write(
            &path,
            b"---\ntitle: \"T\"\ntags: []\ncreated_ts: 1\nupdated_ts: 2\n---\n\n",
        )
        .unwrap();
        let e = parse_entry(&path).unwrap();
        assert_eq!(e.title, "T");
        assert_eq!(e.body, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_entry_rejects_no_frontmatter() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("weave-mem-parse2-{}", crate::model::now()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.md");
        std::fs::write(&path, "no frontmatter here").unwrap();
        assert!(parse_entry(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_traversal_rejected() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        assert!(memory_write(&scope, "../../../etc/passwd", "T", &[], "B").is_err());
        cleanup(&base);
    }

    #[test]
    fn oversized_body_rejected() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(memory_write(&scope, "big", "T", &[], &big).is_err());
        cleanup(&base);
    }

    #[test]
    fn bad_tag_chars_stripped() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        memory_write(&scope, "tags", "T", &["foo;rm -rf".into()], "B").unwrap();
        let e = memory_read(&scope, "tags").unwrap();
        assert_eq!(e.tags, vec!["foorm-rf"]);
        cleanup(&base);
    }

    #[test]
    fn build_context_prefix_smoke() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let scope = MemoryScope::Global;
        memory_write(
            &scope,
            "patterns",
            "Patterns",
            &["rust".into()],
            "Always use types.",
        )
        .unwrap();
        let prefix = build_context_prefix("me", "default", "I love rust", 3);
        assert!(prefix.contains("<weave-memory>"));
        assert!(prefix.contains("Patterns"));
        cleanup(&base);
    }

    #[test]
    fn build_context_prefix_no_match_returns_empty() {
        let _guard = FS_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let base = tmp_memory_dir();
        let prefix = build_context_prefix("me", "default", "xyz abc", 3);
        assert_eq!(prefix, "");
        cleanup(&base);
    }
}
