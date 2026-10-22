//! The repository connector (`kind = "repo"`, docs/CONTROL_PLANE.md
//! section 13): a monorepo's packages, their internal dependencies and
//! their owners, mapped into the `builtin:code` schema.
//!
//! It reads **metadata only**: manifests (`Cargo.toml`, `package.json`,
//! `pnpm-workspace.yaml`, `go.mod`, `go.work`, `pyproject.toml`) and
//! `CODEOWNERS`, never source files. The source is a local directory (a
//! working tree) or a git URL (`https://`, `ssh://` or `user@host:path`),
//! cloned shallowly with the `git` binary into a cache directory.
//!
//! A crawl is incremental: when the commit (a git URL) or the stamp of the
//! manifests' sizes and modification times (a directory) and the
//! connector's options are those of the last crawl, it does nothing.
//! Otherwise the mapped entities go through [`catalog::plan_changes`] with
//! the stored fingerprints, so only changed packages and links are written
//! and packages that disappeared are deleted, in batches through the
//! audited write path.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use onto_core::authority::Weights;
use onto_core::catalog::{self, Fingerprints, Mapped};
use onto_core::time::{new_id, now_ms};
use onto_core::{
    ActionContext, Dir, EntityInput, Error, EventInput, LinkInput, Mutation, Ontology, Provenance, Result,
};

use super::{ConnectorSpec, CrawlReport, apply_batches, connector_principal, load_state, save_state};

/// Relations a repository crawl owns: links of these that the repository no
/// longer has are removed.
pub const MANAGED_RELS: &[&str] = &["depends_on", "owns", "in_repository"];

/// Directories never read.
pub const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", "vendor", "dist", "build", ".venv"];

/// Directory levels below the root that are read.
pub const MAX_DEPTH: usize = 12;

/// Directory entries read at most per crawl.
pub const MAX_ENTRIES: usize = 100_000;

/// Larger manifests are skipped.
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// How long one `git` invocation may take.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Where CODEOWNERS is looked for, in order; the first found is used.
pub const CODEOWNERS: &[&str] = &[".github/CODEOWNERS", "CODEOWNERS", "docs/CODEOWNERS"];

const MANIFESTS: &[&str] =
    &["Cargo.toml", "package.json", "pnpm-workspace.yaml", "go.mod", "go.work", "pyproject.toml", "tsconfig.json"];

/// The `repo` object of a [`ConnectorSpec`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoOptions {
    /// Branch to clone (git URLs; default: the remote's default branch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Only packages whose directory matches one of these gitignore-style patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
    /// Directories to skip (gitignore-style patterns).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// CODEOWNERS owner (`@org/team`, `@user`, an email) → an existing
    /// `team:…` or `person:…` id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub owners: BTreeMap<String, String>,
}

impl RepoOptions {
    pub fn validate(&self) -> Result<()> {
        if let Some(b) = &self.branch {
            check_branch(b)?;
        }
        if self.include.len() > 100 || self.exclude.len() > 100 {
            return Err(Error::invalid("repo: at most 100 include and 100 exclude patterns"));
        }
        for p in self.include.iter().chain(&self.exclude) {
            if p.trim().is_empty() || p.len() > 200 {
                return Err(Error::invalid(format!("repo: pattern '{p}' must be 1 to 200 characters")));
            }
        }
        for (owner, id) in &self.owners {
            if owner.trim().is_empty() {
                return Err(Error::invalid("repo.owners: an empty owner"));
            }
            if !(id.starts_with("team:") || id.starts_with("person:")) || id.len() < 6 {
                return Err(Error::invalid(format!(
                    "repo.owners: {owner} maps to '{id}'; owners are team:… or person:… ids"
                )));
            }
        }
        Ok(())
    }
}

/// A branch name we pass to git: no option, no ref syntax tricks.
fn check_branch(b: &str) -> Result<()> {
    let ok = !b.is_empty()
        && b.len() <= 200
        && !b.starts_with('-')
        && !b.starts_with('/')
        && !b.contains("..")
        && !b.contains("@{")
        && b.chars().all(|c| c.is_ascii_alphanumeric() || "._/-+".contains(c));
    if ok { Ok(()) } else { Err(Error::invalid(format!("repo: '{b}' is not a branch name git accepts here"))) }
}

// ── git URLs ─────────────────────────────────────────────────────────────

/// How a git URL is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitScheme {
    Https,
    /// `ssh://…` or the scp-like `user@host:path`.
    Ssh,
    /// `file://…`: tests only ([`testing_allow_file_urls`]).
    File,
}

/// A git URL the connector accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitUrl {
    pub url: String,
    pub scheme: GitScheme,
    /// Empty for `file://`.
    pub host: String,
    pub port: Option<u16>,
}

static ALLOW_FILE_URLS: AtomicBool = AtomicBool::new(false);
static CACHE_DIR: parking_lot::Mutex<Option<PathBuf>> = parking_lot::Mutex::new(None);

/// Tests only: accept `file://` URLs (a local bare repository) and let git
/// use the file protocol. Never set by the server or the CLI.
#[doc(hidden)]
pub fn testing_allow_file_urls(on: bool) {
    ALLOW_FILE_URLS.store(on, Ordering::SeqCst);
}

/// Tests only: clone into this directory instead of the user cache.
#[doc(hidden)]
pub fn testing_cache_dir(dir: Option<PathBuf>) {
    *CACHE_DIR.lock() = dir;
}

/// Is `s` written as a URL (`scheme://…` or `user@host:path`) rather than a
/// filesystem path?
pub fn looks_like_url(s: &str) -> bool {
    let s = s.trim();
    s.contains("://") || scp_like(s).is_some()
}

/// `user@host:path` → (user, host, path).
fn scp_like(s: &str) -> Option<(&str, &str, &str)> {
    if s.contains("://") {
        return None;
    }
    let (user, rest) = s.split_once('@')?;
    let (host, path) = rest.split_once(':')?;
    let user_ok = !user.is_empty() && user.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    (user_ok && host_ok(host) && !path.is_empty()).then_some((user, host, path))
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && !h.starts_with('-')
        && !h.starts_with('.')
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Accept `https://host[:port]/path`, `ssh://[user@]host[:port]/path` and
/// `user@host:path`; refuse everything else (`ext::`, `file://`, `http://`,
/// `git://`, options, credentials, whitespace).
pub fn parse_git_url(raw: &str) -> Result<GitUrl> {
    let s = raw.trim();
    let bad = |why: &str| Err(Error::invalid(format!("git URL '{s}': {why}")));
    if s.is_empty() {
        return bad("empty");
    }
    if s.starts_with('-') {
        return bad("it may not start with '-'");
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad("it may not contain whitespace or control characters");
    }
    if s.split_once("::").is_some_and(|(t, _)| t.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))) {
        return bad("transport helpers (ext::, fd::, …) are refused");
    }
    if s.len() > 2000 {
        return bad("longer than 2000 characters");
    }
    if let Some((_, host, path)) = scp_like(s) {
        if path.starts_with('-') {
            return bad("the path may not start with '-'");
        }
        return Ok(GitUrl { url: s.to_string(), scheme: GitScheme::Ssh, host: host.to_ascii_lowercase(), port: None });
    }
    let Some((scheme, rest)) = s.split_once("://") else {
        return bad("use https://…, ssh://… or user@host:path");
    };
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => GitScheme::Https,
        "ssh" => GitScheme::Ssh,
        "file" if ALLOW_FILE_URLS.load(Ordering::SeqCst) => {
            return Ok(GitUrl { url: s.to_string(), scheme: GitScheme::File, host: String::new(), port: None });
        }
        "file" => return bad("file:// is refused; register a local path instead"),
        other => return bad(&format!("scheme {other} is refused (https or ssh)")),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if path.len() < 2 {
        return bad("no repository path");
    }
    if path.contains('?') || path.contains('#') {
        return bad("no query or fragment");
    }
    let hostport = match authority.rsplit_once('@') {
        Some((user, hp)) => {
            if scheme == GitScheme::Https {
                return bad("credentials in the URL are refused; the server's git configuration supplies them");
            }
            if user.is_empty() || !user.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
                return bad("a user name of letters, digits, '.', '_' and '-' only (no password)");
            }
            hp
        }
        None => authority,
    };
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let Some((h, after)) = v6.split_once(']') else { return bad("unclosed [") };
        if h.parse::<std::net::Ipv6Addr>().is_err() {
            return bad("not an IPv6 address in [ ]");
        }
        (h.to_string(), after.strip_prefix(':'))
    } else {
        match hostport.split_once(':') {
            Some((h, p)) => (h.to_ascii_lowercase(), Some(p)),
            None => (hostport.to_ascii_lowercase(), None),
        }
    };
    if !host.contains(':') && !host_ok(&host) {
        return bad("not a valid host");
    }
    let port = match port {
        None => None,
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 => Some(n),
            _ => return bad("not a valid port"),
        },
    };
    Ok(GitUrl { url: s.to_string(), scheme, host, port })
}

// ── the git binary ───────────────────────────────────────────────────────

fn cache_root() -> PathBuf {
    if let Some(d) = CACHE_DIR.lock().clone() {
        return d;
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("onto").join("repos")
}

/// `{cache}/{sha256(url, branch)[..16]}`: one clone per URL and branch (the
/// default branch is its own), since a clone made with `--single-branch`
/// fetches only its branch.
pub fn clone_dir(url: &str, branch: Option<&str>) -> PathBuf {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(url.as_bytes());
    if let Some(b) = branch {
        // A NUL cannot be part of a URL or a branch name.
        h.update([0u8]);
        h.update(b.as_bytes());
    }
    let hex: String = h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect();
    cache_root().join(hex)
}

/// Clone directories in use by a crawl of this process.
static BUSY: parking_lot::Mutex<BTreeSet<PathBuf>> = parking_lot::Mutex::new(BTreeSet::new());
static FREED: parking_lot::Condvar = parking_lot::Condvar::new();

/// A clone directory held for one crawl, from the checkout to the last
/// read of its tree: a crawl of the same URL and branch by another
/// connector, tenant or thread of this process waits for it, and so does
/// one in another process sharing the cache (an exclusive lock on
/// `{dir}.lock`).
pub struct DirLock {
    dir: PathBuf,
    file: Option<std::fs::File>,
}

impl DirLock {
    pub fn acquire(dir: &Path) -> Result<Self> {
        {
            let mut busy = BUSY.lock();
            while busy.contains(dir) {
                FREED.wait(&mut busy);
            }
            busy.insert(dir.to_path_buf());
        }
        // From here on, dropping `held` frees the directory again.
        let mut held = DirLock { dir: dir.to_path_buf(), file: None };
        let parent = dir.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| Error::Other(format!("creating {}: {e}", parent.display())))?;
        let path = dir.with_extension("lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| Error::Other(format!("opening {}: {e}", path.display())))?;
        file.lock().map_err(|e| Error::Other(format!("locking {}: {e}", path.display())))?;
        held.file = Some(file);
        Ok(held)
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        // The file lock goes with the file; then the in-process claim.
        self.file = None;
        BUSY.lock().remove(&self.dir);
        FREED.notify_all();
    }
}

/// How git may reach a connector's remote: `-c` settings for every git
/// command of a crawl.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reach {
    pub config: Vec<String>,
}

/// The SSRF rule for a git URL that a project credential registered
/// ([`super::ConnectorSpec::restricted`]), checked when the connector is
/// registered and again before every crawl: unless `allow_private`, only
/// `https://`, and the host must resolve only to public addresses. Blocking
/// (it resolves the host).
///
/// The returned settings keep git on what was checked: redirects are never
/// followed (`http.followRedirects=false`), and without `allow_private`
/// curl connects only to the address checked (`http.curloptResolve`, git
/// 2.37 or later), so a name that resolves differently when git connects
/// cannot reach another address.
pub fn restricted_reach(url: &GitUrl, allow_private: bool) -> Result<Reach> {
    let mut config = vec!["http.followRedirects=false".to_string()];
    if allow_private {
        return Ok(Reach { config });
    }
    if url.scheme != GitScheme::Https {
        return Err(Error::invalid(format!(
            "git URL '{}': only https:// URLs may be registered by API keys and console sessions \
             (ssh:// needs console.allow_private_urls)",
            url.url
        )));
    }
    let port = url.port.unwrap_or(443);
    let addrs = crate::net::resolve_public_blocking(&format!("git URL '{}'", url.url), &url.host, port)?;
    if url.host.parse::<std::net::IpAddr>().is_err() {
        let ip = match addrs[0] {
            std::net::IpAddr::V6(v6) => format!("[{v6}]"),
            v4 => v4.to_string(),
        };
        config.push(format!("http.curloptResolve={}:{port}:{ip}", url.host));
    }
    Ok(Reach { config })
}

/// Run git with the protocol allow-list, the settings of `reach`, no
/// terminal prompts and a timeout.
fn git_reaching(reach: &Reach, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "protocol.allow=never", "-c", "protocol.https.allow=always", "-c", "protocol.ssh.allow=always"]);
    if ALLOW_FILE_URLS.load(Ordering::SeqCst) {
        cmd.args(["-c", "protocol.file.allow=always"]);
    }
    for c in &reach.config {
        cmd.arg("-c").arg(c);
    }
    if let Some(dir) = cwd {
        cmd.arg("-C").arg(dir);
    }
    cmd.args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let verb = args.first().copied().unwrap_or("git");
    let mut child = cmd.spawn().map_err(|e| Error::Unavailable(format!("git {verb}: cannot run git: {e}")))?;
    let pipe = |r: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = pipe(child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let err = pipe(child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > GIT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Unavailable(format!("git {verb} timed out after {} s", GIT_TIMEOUT.as_secs())));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(Error::Unavailable(format!("git {verb}: {e}"))),
        }
    };
    let stdout = String::from_utf8_lossy(&out.join().unwrap_or_default()).to_string();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).trim().to_string();
    if !status.success() {
        let clipped: String = stderr.chars().take(500).collect();
        return Err(Error::Unavailable(format!("git {verb} failed: {clipped}")));
    }
    Ok(stdout.trim().to_string())
}

/// A checked-out copy of a git URL: cloned on the first crawl, fetched and
/// reset to `FETCH_HEAD` later, always naming the ref (`HEAD`, the remote's
/// default branch, when no branch is set). Returns the directory, the
/// commit and the branch. The caller holds the directory's [`DirLock`].
fn checkout(url: &GitUrl, branch: Option<&str>, reach: &Reach) -> Result<(PathBuf, String, Option<String>)> {
    let git = |args: &[&str], cwd: Option<&Path>| git_reaching(reach, args, cwd);
    let dir = clone_dir(&url.url, branch);
    let fetched = dir.join(".git").is_dir()
        && git(&["fetch", "--depth", "1", "--", "origin", branch.unwrap_or("HEAD")], Some(&dir))
            .and_then(|_| git(&["reset", "--hard", "FETCH_HEAD"], Some(&dir)))
            .is_ok();
    if !fetched {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .map_err(|e| Error::Other(format!("removing the stale clone {}: {e}", dir.display())))?;
        }
        let parent = dir.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| Error::Other(format!("creating {}: {e}", parent.display())))?;
        let tmp = parent.join(format!(".{}", new_id("clone")));
        let tmp_s = tmp.to_string_lossy().to_string();
        let mut args = vec!["clone", "--depth", "1", "--filter=blob:none", "--single-branch", "--quiet"];
        if let Some(b) = branch {
            args.extend(["--branch", b]);
        }
        args.extend(["--", url.url.as_str(), tmp_s.as_str()]);
        if let Err(e) = git(&args, None) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, &dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp);
            Error::Other(format!("moving the clone into {}: {e}", dir.display()))
        })?;
    }
    let commit = git(&["rev-parse", "HEAD"], Some(&dir))?;
    // The default-branch clone is its own directory, so its local branch is
    // the branch it was cloned from, never one set on another connector.
    let head = git(&["rev-parse", "--abbrev-ref", "HEAD"], Some(&dir)).ok().filter(|b| b != "HEAD");
    Ok((dir, commit, branch.map(str::to_string).or(head)))
}

/// The commit and branch of a local working tree, read from `.git` without
/// running git (`None` when it is not a git checkout).
fn local_head(root: &Path) -> (Option<String>, Option<String>) {
    let mut git_dir = root.join(".git");
    if git_dir.is_file() {
        // A worktree or submodule: "gitdir: <path>".
        let Ok(text) = std::fs::read_to_string(&git_dir) else { return (None, None) };
        let Some(p) = text.trim().strip_prefix("gitdir:") else { return (None, None) };
        let p = PathBuf::from(p.trim());
        git_dir = if p.is_absolute() { p } else { root.join(p) };
    }
    let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) else { return (None, None) };
    let head = head.trim();
    let Some(r) = head.strip_prefix("ref:").map(str::trim) else {
        return (Some(head.to_string()).filter(|h| h.len() >= 40), None);
    };
    let branch = r.strip_prefix("refs/heads/").map(str::to_string);
    // A linked worktree keeps its refs in the common directory.
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|c| git_dir.join(c.trim()))
        .unwrap_or_else(|| git_dir.clone());
    let loose = [git_dir.join(r), common.join(r)].into_iter().find_map(|p| std::fs::read_to_string(p).ok());
    let commit = loose.map(|c| c.trim().to_string()).or_else(|| {
        std::fs::read_to_string(common.join("packed-refs")).ok().and_then(|packed| {
            packed.lines().find_map(|l| l.split_once(' ').filter(|(_, name)| *name == r).map(|(c, _)| c.to_string()))
        })
    });
    (commit, branch)
}

// ── patterns ─────────────────────────────────────────────────────────────

/// One path segment against a glob segment: `*`, `?`, `[abc]`, `[a-z]`,
/// `[!x]`, `\` escapes.
fn seg_match(p: &[char], s: &[char]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| seg_match(&p[1..], &s[i..])),
        Some('?') => !s.is_empty() && seg_match(&p[1..], &s[1..]),
        Some('[') => {
            let Some(close) = p.iter().skip(1).position(|c| *c == ']').map(|i| i + 1) else {
                return s.first() == Some(&'[') && seg_match(&p[1..], &s[1..]);
            };
            let Some(c) = s.first() else { return false };
            let mut set = &p[1..close];
            let negate = matches!(set.first(), Some('!') | Some('^'));
            if negate {
                set = &set[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == '-' {
                    hit |= set[i] <= *c && *c <= set[i + 2];
                    i += 3;
                } else {
                    hit |= set[i] == *c;
                    i += 1;
                }
            }
            hit != negate && seg_match(&p[close + 1..], &s[1..])
        }
        Some('\\') if p.len() > 1 => s.first() == Some(&p[1]) && seg_match(&p[2..], &s[1..]),
        Some(c) => s.first() == Some(c) && seg_match(&p[1..], &s[1..]),
    }
}

fn segs_match(p: &[&str], s: &[&str]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(&"**") => (0..=s.len()).any(|i| segs_match(&p[1..], &s[i..])),
        Some(seg) => {
            !s.is_empty() && {
                let pc: Vec<char> = seg.chars().collect();
                let sc: Vec<char> = s[0].chars().collect();
                seg_match(&pc, &sc) && segs_match(&p[1..], &s[1..])
            }
        }
    }
}

/// A path glob anchored at both ends: `*` and `?` within a segment, `**`
/// for any number of segments. `./` prefixes and trailing `/` are ignored.
pub fn path_glob(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches("./").trim_end_matches('/');
    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let s: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    segs_match(&p, &s)
}

/// Does a gitignore-style pattern (CODEOWNERS, `include`, `exclude`) cover
/// the directory `dir` (relative, `""` for the root)? A leading `/` or a
/// `/` inside anchors it at the root, otherwise it matches at any depth; a
/// trailing `/` names a directory; a pattern that matches a directory
/// covers everything inside it. The root is covered by `*`, `**` and `/`.
pub fn covers(pattern: &str, dir: &str) -> bool {
    let p = pattern.trim();
    let bare = p.trim_start_matches('/').trim_end_matches('/');
    if matches!(bare, "" | "*" | "**") {
        return true;
    }
    if dir.is_empty() {
        return false;
    }
    let anchored = p.starts_with('/') || bare.contains('/');
    let pat = if anchored { bare.to_string() } else { format!("**/{bare}") };
    let parts: Vec<&str> = dir.split('/').collect();
    (1..=parts.len()).any(|n| path_glob(&pat, &parts[..n].join("/")))
}

// ── CODEOWNERS ───────────────────────────────────────────────────────────

/// One CODEOWNERS rule: a pattern and its owners (none: explicitly unowned).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerRule {
    pub pattern: String,
    pub owners: Vec<String>,
}

/// Parse CODEOWNERS: comments, blank lines and section headers (`[Docs]`)
/// are skipped; `\#` and `\ ` escape.
pub fn parse_codeowners(text: &str) -> Vec<OwnerRule> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') || line.starts_with("^[") {
            continue;
        }
        // Split on unescaped whitespace; an unescaped '#' starts a comment.
        let mut tokens: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        if n != '#' && n != ' ' {
                            cur.push('\\');
                        }
                        cur.push(n);
                    }
                }
                '#' => break,
                c if c.is_whitespace() => {
                    if !cur.is_empty() {
                        tokens.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            tokens.push(cur);
        }
        let mut it = tokens.into_iter();
        if let Some(pattern) = it.next() {
            out.push(OwnerRule { pattern, owners: it.filter(|o| o.contains('@')).collect() });
        }
    }
    out
}

/// The owners of a directory: those of the last rule that covers it.
pub fn owners_of<'a>(rules: &'a [OwnerRule], dir: &str) -> &'a [String] {
    rules.iter().rev().find(|r| covers(&r.pattern, dir)).map(|r| r.owners.as_slice()).unwrap_or(&[])
}

fn id_part(s: &str) -> String {
    s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect()
}

/// The entity id of a CODEOWNERS owner: `team:{org}-{team}` for
/// `@org/team`, `person:gh-{user}` for `@user`, `person:{local}` for an
/// email, unless `map` (owner → id, case-insensitive) names one.
pub fn owner_id(owner: &str, map: &BTreeMap<String, String>) -> Option<String> {
    if let Some((_, id)) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case(owner)) {
        return Some(id.clone());
    }
    if let Some(handle) = owner.strip_prefix('@') {
        return match handle.split_once('/') {
            Some((org, team)) if !org.is_empty() && !team.is_empty() => {
                Some(format!("team:{}-{}", id_part(org), id_part(team)))
            }
            Some(_) => None,
            None if !handle.is_empty() => Some(format!("person:gh-{}", id_part(handle))),
            None => None,
        };
    }
    let (local, domain) = owner.split_once('@')?;
    (!local.is_empty() && domain.contains('.')).then(|| {
        let l: String =
            local.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
        format!("person:{l}")
    })
}

// ── discovery ────────────────────────────────────────────────────────────

/// The files of a tree the connector looks at, and what it skipped.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    /// Manifests by path relative to the root: (size, modified ms).
    pub files: BTreeMap<String, (u64, i64)>,
    pub entries: usize,
    pub notes: Vec<String>,
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Walk the tree from `root` without following symlinks: skip
/// [`SKIP_DIRS`], hidden directories and directories `exclude` covers,
/// read at most [`MAX_DEPTH`] levels and [`MAX_ENTRIES`] entries, and
/// keep the manifests.
pub fn walk(root: &Path, exclude: &[String]) -> Result<Tree> {
    let mut tree = Tree::default();
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    let mut deep = false;
    while let Some((abs, rel, depth)) = stack.pop() {
        let rd = match std::fs::read_dir(&abs) {
            Ok(rd) => rd,
            Err(e) if rel.is_empty() => {
                return Err(Error::invalid(format!("repository {}: {e}", root.display())));
            }
            Err(e) => {
                tree.notes.push(format!("{rel}: not read: {e}"));
                continue;
            }
        };
        let mut entries: Vec<(String, std::fs::FileType)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| Some((e.file_name().into_string().ok()?, e.file_type().ok()?)))
            .collect();
        entries.sort_by(|a, b| b.0.cmp(&a.0));
        for (name, ft) in entries {
            tree.entries += 1;
            if tree.entries > MAX_ENTRIES {
                tree.notes.push(format!("more than {MAX_ENTRIES} directory entries: the rest of the tree is not read"));
                return Ok(tree);
            }
            if ft.is_symlink() {
                continue;
            }
            let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            if ft.is_dir() {
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if exclude.iter().any(|p| covers(p, &child)) {
                    continue;
                }
                if depth + 1 > MAX_DEPTH {
                    if !deep {
                        tree.notes.push(format!("directories deeper than {MAX_DEPTH} levels are not read"));
                        deep = true;
                    }
                    continue;
                }
                stack.push((abs.join(&name), child, depth + 1));
            } else if ft.is_file()
                && MANIFESTS.contains(&name.as_str())
                && let Ok(md) = std::fs::symlink_metadata(abs.join(&name))
            {
                tree.files.insert(child, (md.len(), mtime_ms(&md)));
            }
        }
    }
    Ok(tree)
}

/// Read a manifest of the tree (`None` with a note when too large or unreadable).
fn read_file(root: &Path, rel: &str, notes: &mut Vec<String>) -> Option<String> {
    let path = root.join(rel);
    let md = std::fs::symlink_metadata(&path).ok().filter(|m| m.is_file())?;
    if md.len() > MAX_MANIFEST_BYTES {
        notes.push(format!("{rel}: larger than 1 MB, skipped"));
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) => {
            notes.push(format!("{rel}: not read: {e}"));
            None
        }
    }
}

/// The first CODEOWNERS file found, with its size and modification time.
fn find_codeowners(root: &Path) -> Option<(String, u64, i64)> {
    CODEOWNERS.iter().find_map(|rel| {
        let md = std::fs::symlink_metadata(root.join(rel)).ok().filter(|m| m.is_file())?;
        Some((rel.to_string(), md.len(), mtime_ms(&md)))
    })
}

/// A package's ecosystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ecosystem {
    Cargo,
    Npm,
    Go,
    Python,
}

impl Ecosystem {
    pub fn as_str(self) -> &'static str {
        match self {
            Ecosystem::Cargo => "cargo",
            Ecosystem::Npm => "npm",
            Ecosystem::Go => "go",
            Ecosystem::Python => "python",
        }
    }
}

/// What a dependency names before it is resolved to a package.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepTarget {
    /// A package directory (Cargo `path`, Go `replace … => ../dir`).
    Dir(String),
    /// A package name (npm, Python normalized, a Go module path).
    Name(String),
}

/// One package as discovered.
#[derive(Clone, Debug, PartialEq)]
pub struct Package {
    pub ecosystem: Ecosystem,
    pub name: String,
    /// Directory relative to the root (`""` for the root).
    pub dir: String,
    pub manifest: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub private: bool,
    pub language: &'static str,
    /// Dependencies with their kind: `normal`, `dev`, `build`, `peer`, `optional`.
    pub deps: Vec<(DepTarget, &'static str)>,
}

fn parent_dir(rel: &str) -> String {
    rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default()
}

/// `dir` joined with a relative path, normalized; `None` outside the root.
fn join_rel(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for seg in rel.split(['/', '\\']) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// Is `dir` the directory `root` or inside it?
fn within(dir: &str, root: &str) -> bool {
    root.is_empty() || dir == root || dir.starts_with(&format!("{root}/"))
}

/// `dir` relative to `root` (which contains it).
fn rel_to(dir: &str, root: &str) -> String {
    if root.is_empty() {
        dir.to_string()
    } else {
        dir.strip_prefix(root).unwrap_or(dir).trim_start_matches('/').to_string()
    }
}

fn manifests<'a>(tree: &'a Tree, file: &'a str) -> impl Iterator<Item = &'a String> + 'a {
    tree.files.keys().filter(move |p| p.rsplit('/').next() == Some(file))
}

/// Workspace members: directories (with a manifest) under `root` that a
/// pattern of `include` matches and no pattern of `exclude` does.
fn members(dirs: &[String], root: &str, include: &[String], exclude: &[String]) -> BTreeSet<String> {
    dirs.iter()
        .filter(|d| within(d, root) && *d != root)
        .filter(|d| {
            let r = rel_to(d, root);
            include.iter().any(|p| path_glob(p, &r)) && !exclude.iter().any(|p| path_glob(p, &r))
        })
        .cloned()
        .collect()
}

fn toml_str<'a>(t: &'a toml::Table, path: &[&str]) -> Option<&'a str> {
    let mut cur = t;
    for (i, key) in path.iter().enumerate() {
        let v = cur.get(*key)?;
        if i + 1 == path.len() {
            return v.as_str();
        }
        cur = v.as_table()?;
    }
    None
}

fn toml_table<'a>(t: &'a toml::Table, path: &[&str]) -> Option<&'a toml::Table> {
    let mut cur = t;
    for key in path {
        cur = cur.get(*key)?.as_table()?;
    }
    Some(cur)
}

fn toml_strings(v: Option<&toml::Value>) -> Vec<String> {
    v.and_then(toml::Value::as_array)
        .map(|a| a.iter().filter_map(toml::Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn parse_toml(root: &Path, rel: &str, notes: &mut Vec<String>) -> Option<toml::Table> {
    let text = read_file(root, rel, notes)?;
    match text.parse::<toml::Table>() {
        Ok(t) => Some(t),
        Err(e) => {
            notes.push(format!("{rel}: not valid TOML: {}", e.message()));
            None
        }
    }
}

/// Cargo: workspaces (`[workspace] members` minus `exclude`, plus the
/// workspace's own `[package]`) and packages outside any workspace.
fn cargo(root: &Path, tree: &Tree, notes: &mut Vec<String>) -> Vec<Package> {
    let parsed: BTreeMap<String, toml::Table> = manifests(tree, "Cargo.toml")
        .filter_map(|rel| parse_toml(root, rel, notes).map(|t| (parent_dir(rel), t)))
        .collect();
    let dirs: Vec<String> = parsed
        .iter()
        .filter(|(_, t)| t.get("package").is_some_and(toml::Value::is_table))
        .map(|(d, _)| d.clone())
        .collect();
    // Package directory → its workspace root.
    let mut ws_of: BTreeMap<String, String> = BTreeMap::new();
    let ws_roots: Vec<&String> =
        parsed.iter().filter(|(_, t)| toml_table(t, &["workspace"]).is_some()).map(|(d, _)| d).collect();
    for ws in &ws_roots {
        let w = toml_table(&parsed[*ws], &["workspace"]).cloned().unwrap_or_default();
        for m in members(&dirs, ws, &toml_strings(w.get("members")), &toml_strings(w.get("exclude"))) {
            ws_of.entry(m).or_insert_with(|| (*ws).clone());
        }
        if dirs.contains(ws) {
            ws_of.insert((*ws).clone(), (*ws).clone());
        }
    }
    let mut out = Vec::new();
    for dir in &dirs {
        let ws = ws_of.get(dir);
        // Inside a workspace's directory but not one of its members: not built by it.
        if ws.is_none() && ws_roots.iter().any(|w| within(dir, w)) {
            continue;
        }
        let t = &parsed[dir];
        let ws_t = ws.and_then(|w| parsed.get(w));
        let inherited = |field: &str| -> Option<String> {
            let p = t.get("package")?.as_table()?;
            match p.get(field)? {
                toml::Value::String(s) => Some(s.clone()),
                toml::Value::Table(x) if x.get("workspace").and_then(toml::Value::as_bool) == Some(true) => {
                    ws_t.and_then(|w| toml_str(w, &["workspace", "package", field])).map(str::to_string)
                }
                _ => None,
            }
        };
        let Some(name) = toml_str(t, &["package", "name"]) else { continue };
        let private =
            matches!(toml_table(t, &["package"]).and_then(|p| p.get("publish")), Some(toml::Value::Boolean(false)))
                || toml_table(t, &["package"])
                    .and_then(|p| p.get("publish"))
                    .and_then(toml::Value::as_array)
                    .is_some_and(|a| a.is_empty());
        let mut deps = Vec::new();
        let mut tables: Vec<(&toml::Table, &'static str)> = Vec::new();
        for (key, kind) in [("dependencies", "normal"), ("dev-dependencies", "dev"), ("build-dependencies", "build")] {
            if let Some(d) = toml_table(t, &[key]) {
                tables.push((d, kind));
            }
            if let Some(targets) = toml_table(t, &["target"]) {
                for target in targets.values().filter_map(toml::Value::as_table) {
                    if let Some(d) = target.get(key).and_then(toml::Value::as_table) {
                        tables.push((d, kind));
                    }
                }
            }
        }
        for (table, kind) in tables {
            for (key, v) in table {
                let Some(entry) = v.as_table() else { continue };
                let target = if let Some(p) = entry.get("path").and_then(toml::Value::as_str) {
                    join_rel(dir, p)
                } else if entry.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    ws.and_then(|w| {
                        let wt = parsed.get(w)?;
                        let p = toml_table(wt, &["workspace", "dependencies"])?
                            .get(key)?
                            .as_table()?
                            .get("path")?
                            .as_str()?;
                        join_rel(w, p)
                    })
                } else {
                    None
                };
                if let Some(d) = target {
                    deps.push((DepTarget::Dir(d), kind));
                }
            }
        }
        out.push(Package {
            ecosystem: Ecosystem::Cargo,
            name: name.to_string(),
            dir: dir.clone(),
            manifest: if dir.is_empty() { "Cargo.toml".into() } else { format!("{dir}/Cargo.toml") },
            version: inherited("version"),
            description: inherited("description"),
            private,
            language: "rust",
            deps,
        });
    }
    out
}

/// The `packages:` list of a pnpm-workspace.yaml (block or flow style).
pub fn pnpm_packages(text: &str) -> Vec<String> {
    let unquote = |s: &str| {
        let s = s.split(" #").next().unwrap_or(s).trim();
        s.trim_matches(|c| c == '\'' || c == '"').to_string()
    };
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        if !indented {
            inside = false;
            if let Some(rest) = trimmed.strip_prefix("packages:") {
                let rest = rest.trim();
                if let Some(flow) = rest.strip_prefix('[') {
                    out.extend(flow.trim_end_matches(']').split(',').map(unquote).filter(|s| !s.is_empty()));
                } else {
                    inside = true;
                }
            }
            continue;
        }
        if inside && let Some(item) = trimmed.strip_prefix('-') {
            let item = unquote(item);
            if !item.is_empty() {
                out.push(item);
            }
        }
    }
    out
}

fn json_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// npm and pnpm: workspaces (`package.json` `workspaces`,
/// `pnpm-workspace.yaml` `packages`) and packages outside any workspace.
fn npm(root: &Path, tree: &Tree, notes: &mut Vec<String>) -> Vec<Package> {
    let parsed: BTreeMap<String, Value> = manifests(tree, "package.json")
        .filter_map(|rel| {
            let text = read_file(root, rel, notes)?;
            match serde_json::from_str::<Value>(&text) {
                Ok(v) if v.is_object() => Some((parent_dir(rel), v)),
                Ok(_) => None,
                Err(e) => {
                    notes.push(format!("{rel}: not valid JSON: {e}"));
                    None
                }
            }
        })
        .collect();
    let mut ws_patterns: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (dir, v) in &parsed {
        let list = match v.get("workspaces") {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            Some(Value::Object(o)) => o
                .get("packages")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if !list.is_empty() {
            ws_patterns.entry(dir.clone()).or_default().extend(list);
        }
    }
    for rel in manifests(tree, "pnpm-workspace.yaml") {
        if let Some(text) = read_file(root, rel, notes) {
            ws_patterns.entry(parent_dir(rel)).or_default().extend(pnpm_packages(&text));
        }
    }
    let dirs: Vec<String> =
        parsed.iter().filter(|(_, v)| v.get("name").is_some_and(Value::is_string)).map(|(d, _)| d.clone()).collect();
    let mut member: BTreeSet<String> = BTreeSet::new();
    for (ws, pats) in &ws_patterns {
        let (exclude, include): (Vec<String>, Vec<String>) = pats.iter().cloned().partition(|p| p.starts_with('!'));
        let exclude: Vec<String> = exclude.iter().map(|p| p.trim_start_matches('!').to_string()).collect();
        member.extend(members(&dirs, ws, &include, &exclude));
    }
    let mut out = Vec::new();
    for dir in &dirs {
        if !member.contains(dir) && ws_patterns.keys().any(|w| within(dir, w)) {
            // A workspace root itself, or inside one without being a member.
            continue;
        }
        let v = &parsed[dir];
        let Some(name) = json_str(v, "name") else { continue };
        let mut deps = Vec::new();
        for (key, kind) in [
            ("dependencies", "normal"),
            ("devDependencies", "dev"),
            ("peerDependencies", "peer"),
            ("optionalDependencies", "optional"),
        ] {
            if let Some(o) = v.get(key).and_then(Value::as_object) {
                deps.extend(o.keys().map(|k| (DepTarget::Name(k.clone()), kind)));
            }
        }
        let ts = tree.files.contains_key(&if dir.is_empty() {
            "tsconfig.json".into()
        } else {
            format!("{dir}/tsconfig.json")
        }) || deps.iter().any(|(d, _)| *d == DepTarget::Name("typescript".into()));
        out.push(Package {
            ecosystem: Ecosystem::Npm,
            name,
            dir: dir.clone(),
            manifest: if dir.is_empty() { "package.json".into() } else { format!("{dir}/package.json") },
            version: json_str(v, "version"),
            description: json_str(v, "description").filter(|d| !d.is_empty()),
            private: v.get("private").and_then(Value::as_bool) == Some(true),
            language: if ts { "typescript" } else { "javascript" },
            deps,
        });
    }
    out
}

/// The directives of a go.mod or go.work: `(verb, argument line)` for
/// single lines and each line of a `verb ( … )` block, comments removed.
fn go_directives(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut block: Option<String> = None;
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if let Some(verb) = &block {
            if line == ")" {
                block = None;
            } else {
                out.push((verb.clone(), line.to_string()));
            }
            continue;
        }
        let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        if rest == "(" {
            block = Some(verb.to_string());
        } else {
            out.push((verb.to_string(), rest.to_string()));
        }
    }
    out
}

fn unquote_go(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('`').to_string()
}

/// Go: modules a `go.work` uses, and every `go.mod` outside a `go.work`'s directory.
fn go(root: &Path, tree: &Tree, notes: &mut Vec<String>) -> Vec<Package> {
    let mut used: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for rel in manifests(tree, "go.work") {
        let Some(text) = read_file(root, rel, notes) else { continue };
        let dir = parent_dir(rel);
        let set = used.entry(dir.clone()).or_default();
        for (verb, arg) in go_directives(&text) {
            if verb == "use"
                && let Some(d) = join_rel(&dir, &unquote_go(&arg))
            {
                set.insert(d);
            }
        }
    }
    let mut out = Vec::new();
    for rel in manifests(tree, "go.mod") {
        let dir = parent_dir(rel);
        let in_work = used.iter().filter(|(w, _)| within(&dir, w)).collect::<Vec<_>>();
        if !in_work.is_empty() && !in_work.iter().any(|(_, set)| set.contains(&dir)) {
            continue;
        }
        let Some(text) = read_file(root, rel, notes) else { continue };
        let directives = go_directives(&text);
        let Some(module) = directives.iter().find(|(v, _)| v == "module").map(|(_, a)| unquote_go(a)) else {
            notes.push(format!("{rel}: no module directive"));
            continue;
        };
        let mut deps = Vec::new();
        for (verb, arg) in &directives {
            match verb.as_str() {
                "require" => {
                    if let Some(path) = arg.split_whitespace().next() {
                        deps.push((DepTarget::Name(unquote_go(path)), "normal"));
                    }
                }
                "replace" => {
                    if let Some((_, to)) = arg.split_once("=>") {
                        let to = unquote_go(to.split_whitespace().next().unwrap_or_default());
                        if (to.starts_with("./") || to.starts_with("../"))
                            && let Some(d) = join_rel(&dir, &to)
                        {
                            deps.push((DepTarget::Dir(d), "normal"));
                        }
                    }
                }
                _ => {}
            }
        }
        out.push(Package {
            ecosystem: Ecosystem::Go,
            name: module,
            dir,
            manifest: rel.clone(),
            version: None,
            description: None,
            private: false,
            language: "go",
            deps,
        });
    }
    out
}

/// PEP 503: lowercase, runs of `-`, `_` and `.` become `-`.
pub fn pep503(name: &str) -> String {
    let mut out = String::new();
    let mut sep = false;
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            sep = true;
        } else {
            if sep && !out.is_empty() {
                out.push('-');
            }
            sep = false;
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// The distribution name of a PEP 508 requirement (`foo[bar]>=1; …` → `foo`).
pub fn pep508_name(req: &str) -> Option<String> {
    let name: String =
        req.trim().chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).collect();
    (!name.is_empty()).then(|| pep503(&name))
}

/// Python: `[tool.uv.workspace]` members (and the workspace's own project),
/// and every other `pyproject.toml` with a `[project]` or `[tool.poetry]` name.
fn python(root: &Path, tree: &Tree, notes: &mut Vec<String>) -> Vec<Package> {
    let parsed: BTreeMap<String, toml::Table> = manifests(tree, "pyproject.toml")
        .filter_map(|rel| parse_toml(root, rel, notes).map(|t| (parent_dir(rel), t)))
        .collect();
    let name_of = |t: &toml::Table| {
        toml_str(t, &["project", "name"]).or_else(|| toml_str(t, &["tool", "poetry", "name"])).map(str::to_string)
    };
    let dirs: Vec<String> = parsed.iter().filter(|(_, t)| name_of(t).is_some()).map(|(d, _)| d.clone()).collect();
    let mut member: BTreeSet<String> = BTreeSet::new();
    let mut ws_roots: Vec<String> = Vec::new();
    for (dir, t) in &parsed {
        if let Some(w) = toml_table(t, &["tool", "uv", "workspace"]) {
            ws_roots.push(dir.clone());
            member.extend(members(&dirs, dir, &toml_strings(w.get("members")), &toml_strings(w.get("exclude"))));
            if dirs.contains(dir) {
                member.insert(dir.clone());
            }
        }
    }
    let mut out = Vec::new();
    for dir in &dirs {
        if !member.contains(dir) && ws_roots.iter().any(|w| within(dir, w)) {
            continue;
        }
        let t = &parsed[dir];
        let Some(name) = name_of(t) else { continue };
        let field =
            |f: &str| toml_str(t, &["project", f]).or_else(|| toml_str(t, &["tool", "poetry", f])).map(str::to_string);
        let mut deps = Vec::new();
        for req in toml_strings(toml_table(t, &["project"]).and_then(|p| p.get("dependencies"))) {
            deps.extend(pep508_name(&req).map(|n| (DepTarget::Name(n), "normal")));
        }
        if let Some(opt) = toml_table(t, &["project", "optional-dependencies"]) {
            for list in opt.values() {
                for req in toml_strings(Some(list)) {
                    deps.extend(pep508_name(&req).map(|n| (DepTarget::Name(n), "optional")));
                }
            }
        }
        if let Some(groups) = toml_table(t, &["dependency-groups"]) {
            for list in groups.values() {
                for req in toml_strings(Some(list)) {
                    deps.extend(pep508_name(&req).map(|n| (DepTarget::Name(n), "dev")));
                }
            }
        }
        let poetry_keys = |path: &[&str], kind: &'static str, deps: &mut Vec<(DepTarget, &'static str)>| {
            if let Some(d) = toml_table(t, path) {
                deps.extend(d.keys().filter(|k| *k != "python").map(|k| (DepTarget::Name(pep503(k)), kind)));
            }
        };
        poetry_keys(&["tool", "poetry", "dependencies"], "normal", &mut deps);
        poetry_keys(&["tool", "poetry", "dev-dependencies"], "dev", &mut deps);
        if let Some(groups) = toml_table(t, &["tool", "poetry", "group"]) {
            for g in groups.values().filter_map(toml::Value::as_table) {
                if let Some(d) = g.get("dependencies").and_then(toml::Value::as_table) {
                    deps.extend(d.keys().filter(|k| *k != "python").map(|k| (DepTarget::Name(pep503(k)), "dev")));
                }
            }
        }
        let private = toml_strings(toml_table(t, &["project"]).and_then(|p| p.get("classifiers")))
            .iter()
            .any(|c| c.starts_with("Private ::"));
        out.push(Package {
            ecosystem: Ecosystem::Python,
            name: pep503(&name),
            dir: dir.clone(),
            manifest: if dir.is_empty() { "pyproject.toml".into() } else { format!("{dir}/pyproject.toml") },
            version: field("version"),
            description: field("description").filter(|d| !d.is_empty()),
            private,
            language: "python",
            deps,
        });
    }
    out
}

/// Every package of the tree, in ecosystem and path order; `include`
/// (when not empty) keeps the packages whose directory it covers.
pub fn discover(root: &Path, tree: &Tree, include: &[String], notes: &mut Vec<String>) -> Vec<Package> {
    let mut all = cargo(root, tree, notes);
    all.extend(npm(root, tree, notes));
    all.extend(go(root, tree, notes));
    all.extend(python(root, tree, notes));
    all.retain(|p| include.is_empty() || include.iter().any(|pat| covers(pat, &p.dir)));
    all.sort_by(|a, b| (a.ecosystem, &a.dir, &a.name).cmp(&(b.ecosystem, &b.dir, &b.name)));
    all
}

/// `@scope/pkg` → `scope.pkg`; lowercase, characters outside `[a-z0-9_.-]` become `-`.
pub fn reduce_name(name: &str) -> String {
    name.trim()
        .trim_start_matches('@')
        .to_lowercase()
        .chars()
        .map(|c| match c {
            '/' => '.',
            c if c.is_ascii_alphanumeric() || "_.-".contains(c) => c,
            _ => '-',
        })
        .collect()
}

pub fn repository_id(connector: &str) -> String {
    format!("repository:{connector}")
}

pub fn package_id(connector: &str, eco: Ecosystem, name: &str) -> String {
    format!("package:{connector}.{}.{}", eco.as_str(), reduce_name(name))
}

/// What a repository crawl found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RepoStats {
    /// The commit crawled (git URLs and local git checkouts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub manifests: usize,
    pub packages: usize,
    /// Ecosystem → packages.
    pub ecosystems: BTreeMap<String, usize>,
    pub dependencies: usize,
    pub owners: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codeowners: Option<String>,
}

/// The entities of one crawl.
pub struct RepoMap {
    pub mapped: Vec<Mapped>,
    pub stats: RepoStats,
    /// Owned entity (package or repository) → its owners.
    pub owned: BTreeMap<String, BTreeSet<String>>,
}

/// Turn discovered packages and CODEOWNERS into entities and links.
/// `existing` says whether an owner id is already in the graph (then it is
/// linked, not written) and `previous` holds the ids the last crawl wrote
/// (owners it created stay managed).
#[allow(clippy::too_many_arguments)]
pub fn map_repo(
    spec: &ConnectorSpec,
    packages: &[Package],
    rules: &[OwnerRule],
    workspace: &str,
    provenance: &Provenance,
    branch: Option<&str>,
    existing: &dyn Fn(&str) -> bool,
    previous: &Fingerprints,
    notes: &mut Vec<String>,
) -> RepoMap {
    let opts = spec.repo.clone().unwrap_or_default();
    let conn = spec.name.as_str();
    let entity = |kind: &str, id: &str, title: &str| EntityInput {
        id: Some(id.to_string()),
        kind: kind.to_string(),
        workspace: Some(workspace.to_string()),
        title: Some(title.to_string()),
        provenance: Some(provenance.clone()),
        ..Default::default()
    };
    let mut stats = RepoStats::default();
    let repo_id = repository_id(conn);
    let mut ids: BTreeMap<(Ecosystem, String), String> = BTreeMap::new();
    let mut by_dir: BTreeMap<(Ecosystem, String), String> = BTreeMap::new();
    let mut kept: Vec<(&Package, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for p in packages {
        let id = package_id(conn, p.ecosystem, &p.name);
        if !seen.insert(id.clone()) {
            notes.push(format!("{}: another package already has the id {id}; skipped", p.manifest));
            continue;
        }
        let key = if p.ecosystem == Ecosystem::Python { pep503(&p.name) } else { p.name.clone() };
        ids.entry((p.ecosystem, key)).or_insert_with(|| id.clone());
        by_dir.entry((p.ecosystem, p.dir.clone())).or_insert_with(|| id.clone());
        kept.push((p, id));
    }
    let mut owned: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut owner_titles: BTreeMap<String, String> = BTreeMap::new();
    let mut owner_ids = |dir: &str, notes: &mut Vec<String>| -> Vec<String> {
        let mut out = Vec::new();
        for o in owners_of(rules, dir) {
            match owner_id(o, &opts.owners) {
                Some(id) => {
                    owner_titles.entry(id.clone()).or_insert_with(|| o.clone());
                    out.push(id);
                }
                None => notes.push(format!("CODEOWNERS: '{o}' is not an owner this connector can map")),
            }
        }
        out.sort();
        out.dedup();
        out
    };

    let mut mapped = Vec::new();
    let mut repo = entity("repository", &repo_id, if spec.title.is_empty() { conn } else { &spec.title });
    repo.props.insert("url".into(), json!(spec.url));
    if let Some(b) = branch {
        repo.props.insert("default_branch".into(), json!(b));
    }
    let root_owners = owner_ids("", notes);
    let mut repo_links = Vec::new();
    for o in &root_owners {
        repo_links.push(LinkInput::new(o.clone(), "owns", repo_id.clone()));
        owned.entry(repo_id.clone()).or_default().insert(o.clone());
    }
    mapped.push(Mapped { input: repo, links: repo_links });

    let order = ["normal", "build", "peer", "optional", "dev"];
    for (p, id) in &kept {
        let title = match p.ecosystem {
            Ecosystem::Go => p.name.rsplit('/').next().unwrap_or(&p.name).to_string(),
            _ => p.name.clone(),
        };
        let mut e = entity("package", id, &title);
        e.props.insert("ecosystem".into(), json!(p.ecosystem.as_str()));
        e.props.insert("path".into(), json!(if p.dir.is_empty() { "." } else { p.dir.as_str() }));
        e.props.insert("manifest".into(), json!(p.manifest));
        e.props.insert("private".into(), json!(p.private));
        e.props.insert("language".into(), json!(p.language));
        if let Some(v) = &p.version {
            e.props.insert("version".into(), json!(v));
        }
        if let Some(d) = &p.description {
            e.props.insert("description".into(), json!(d));
        }
        if p.ecosystem == Ecosystem::Go {
            e.props.insert("module".into(), json!(p.name));
        }
        if title != p.name {
            e.aliases.push(p.name.clone());
        }
        let mut links = vec![LinkInput::new(id.clone(), "in_repository", repo_id.clone())];
        // One link per target; the strongest kind wins (normal over build, … over dev).
        let mut targets: BTreeMap<String, &'static str> = BTreeMap::new();
        for (dep, kind) in &p.deps {
            let to = match dep {
                DepTarget::Dir(d) => by_dir.get(&(p.ecosystem, d.clone())),
                DepTarget::Name(n) => {
                    let key = if p.ecosystem == Ecosystem::Python { pep503(n) } else { n.clone() };
                    ids.get(&(p.ecosystem, key))
                }
            };
            let Some(to) = to.filter(|to| *to != id) else { continue };
            let rank = |k: &str| order.iter().position(|o| *o == k).unwrap_or(order.len());
            let slot = targets.entry(to.clone()).or_insert(kind);
            if rank(kind) < rank(slot) {
                *slot = kind;
            }
        }
        for (to, kind) in targets {
            let mut l = LinkInput::new(id.clone(), "depends_on", to);
            l.props.insert("kind".into(), json!(kind));
            links.push(l);
            stats.dependencies += 1;
        }
        for o in owner_ids(&p.dir, notes) {
            links.push(LinkInput::new(o.clone(), "owns", id.clone()));
            owned.entry(id.clone()).or_default().insert(o);
        }
        *stats.ecosystems.entry(p.ecosystem.as_str().to_string()).or_default() += 1;
        stats.packages += 1;
        mapped.push(Mapped { input: e, links });
    }
    // Owners: link to existing teams and people; create the others (and
    // keep managing those this connector created before).
    let mut by_owner: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (target, owners) in &owned {
        for o in owners {
            by_owner.entry(o.clone()).or_default().push(target.clone());
        }
    }
    stats.owners = by_owner.len();
    for (o, targets) in by_owner {
        if existing(&o) && !previous.contains_key(&o) {
            continue;
        }
        let kind = if o.starts_with("team:") { "team" } else { "person" };
        let handle = owner_titles.get(&o).cloned().unwrap_or_else(|| o.clone());
        let mut e = entity(kind, &o, &handle);
        e.aliases.push(handle);
        let links = targets.into_iter().map(|t| LinkInput::new(o.clone(), "owns", t)).collect();
        mapped.push(Mapped { input: e, links });
    }
    RepoMap { mapped, stats, owned }
}

/// Plan a crawl: [`catalog::plan_changes`] with [`MANAGED_RELS`], plus the
/// `owns` links into changed packages and the repository that CODEOWNERS
/// no longer gives (`owns` points into them, so the plan's outgoing check
/// does not see those), and no duplicate links.
pub fn plan(map: RepoMap, previous: &Fingerprints, g: &onto_core::graph::Graph) -> catalog::CrawlPlan {
    let changed: Vec<String> = map
        .mapped
        .iter()
        .filter(|m| matches!(m.input.kind.as_str(), "package" | "repository"))
        .filter(|m| previous.get(m.id()) != Some(&catalog::fingerprint(m)) || !g.contains(m.id()))
        .map(|m| m.id().to_string())
        .collect();
    let mut stale = Vec::new();
    for id in &changed {
        let want = map.owned.get(id);
        for from in g.neighbors(id, "owns", Dir::In) {
            if !want.is_some_and(|w| w.contains(from)) {
                stale.push(Mutation::Unlink(LinkInput::new(from, "owns", id.clone())));
            }
        }
    }
    let mut plan = catalog::plan_changes(map.mapped, previous, g, MANAGED_RELS);
    plan.links_removed += stale.len();
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut muts = stale;
    for m in std::mem::take(&mut plan.mutations) {
        match &m {
            Mutation::Link(l) if !seen.insert((l.from.clone(), l.rel.clone(), l.to.clone())) => {}
            Mutation::Unlink(l)
                if muts.iter().any(
                    |x| matches!(x, Mutation::Unlink(y) if y.from == l.from && y.rel == l.rel && y.to == l.to),
                ) => {}
            _ => muts.push(m),
        }
    }
    plan.mutations = muts;
    plan
}

/// The stamp of what a crawl reads: the commit (or the manifests' sizes
/// and modification times) and the connector's options.
fn stamp(spec: &ConnectorSpec, head: &str) -> String {
    let opts = serde_json::to_string(&json!({
        "name": spec.name, "url": spec.url, "workspace": spec.workspace, "title": spec.title, "repo": spec.repo,
    }))
    .unwrap_or_default();
    format!("{head}/{}", crate::hash::stable_hash(&[&opts]))
}

fn tree_stamp(tree: &Tree, codeowners: Option<&(String, u64, i64)>) -> String {
    let mut parts: Vec<String> = tree.files.iter().map(|(p, (size, m))| format!("{p}:{size}:{m}")).collect();
    if let Some((p, size, m)) = codeowners {
        parts.push(format!("{p}:{size}:{m}"));
    }
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    format!("tree:{}", crate::hash::stable_hash(&refs))
}

/// Crawl a repository connector: check out (git URLs), walk, map and write
/// what changed. Synchronous (git and the filesystem); callers run it off
/// async workers.
///
/// `allow_private` is `console.allow_private_urls`: a restricted spec's git
/// URL is checked again with [`restricted_reach`] before git runs.
pub fn crawl(onto: &Ontology, spec: &ConnectorSpec, started: i64, allow_private: bool) -> Result<CrawlReport> {
    if !onto.schema().object_types.contains_key("package") {
        return Err(Error::invalid(format!(
            "connector {}: the schema has no package type; add builtin:code to [ontology] extend \
             (after builtin:data-catalog)",
            spec.name
        )));
    }
    let opts = spec.repo.clone().unwrap_or_default();
    let mut state = load_state(onto, &spec.name)?;
    let mut held = None;
    let (root, commit, branch, head) = if looks_like_url(&spec.url) {
        let url = parse_git_url(&spec.url)?;
        let reach = if spec.restricted {
            restricted_reach(&url, allow_private)
                .map_err(|e| Error::invalid(format!("connector {}: {e}", spec.name)))?
        } else {
            Reach::default()
        };
        held = Some(DirLock::acquire(&clone_dir(&url.url, opts.branch.as_deref()))?);
        let (dir, commit, branch) = checkout(&url, opts.branch.as_deref(), &reach)?;
        let head = format!("commit:{commit}");
        (dir, Some(commit), branch, Some(head))
    } else {
        if spec.restricted {
            return Err(Error::invalid(format!(
                "connector {}: a connector registered by an API key or a console session cannot read a directory",
                spec.name
            )));
        }
        let root = PathBuf::from(spec.url.trim());
        if !root.is_dir() {
            return Err(Error::invalid(format!("connector {}: {} is not a directory", spec.name, root.display())));
        }
        let (commit, branch) = local_head(&root);
        (root, commit, branch, None)
    };
    let tree = walk(&root, &opts.exclude)?;
    let owners_file = find_codeowners(&root);
    let stamp = stamp(spec, &head.unwrap_or_else(|| tree_stamp(&tree, owners_file.as_ref())));
    let all_there = onto.read(|g, _| state.fingerprints.keys().all(|id| g.contains(id)));
    let mut report = CrawlReport {
        connector: spec.name.clone(),
        source: repository_id(&spec.name),
        started_at: started,
        ..Default::default()
    };
    if state.stamp.as_deref() == Some(stamp.as_str()) && all_there && state.last_error.is_none() {
        report.unchanged = state.fingerprints.len();
        report.repo = state.last.as_ref().and_then(|l| l.repo.clone());
        report.notes.push("unchanged since the last crawl: nothing written".into());
        report.ms = (now_ms() - started).max(0) as u64;
        state.last_crawl_at = started;
        state.last_success_at = now_ms();
        state.crawls += 1;
        save_state(onto, &spec.name, &state)?;
        return Ok(report);
    }
    let mut notes = tree.notes.clone();
    let packages = discover(&root, &tree, &opts.include, &mut notes);
    let rules = match &owners_file {
        Some((rel, _, _)) => read_file(&root, rel, &mut notes).map(|t| parse_codeowners(&t)).unwrap_or_default(),
        None => Vec::new(),
    };
    // Everything is read from the clone: another crawl may check it out now.
    drop(held);
    let ws = spec.workspace.clone().unwrap_or_else(|| onto.options().default_workspace.clone());
    let run = new_id("crawl");
    let provenance = Provenance {
        source: "connector:repo".into(),
        run: Some(run),
        external_ref: Some(match &commit {
            Some(c) => format!("{}@{c}", spec.url.trim()),
            None => spec.url.trim().to_string(),
        }),
        at: started,
        connector: Some(spec.name.clone()),
        ..Default::default()
    };
    let previous = state.fingerprints.clone();
    let plan = onto.read(|g, _| {
        let map = map_repo(
            spec,
            &packages,
            &rules,
            &ws,
            &provenance,
            branch.as_deref(),
            &|id| g.contains(id),
            &previous,
            &mut notes,
        );
        let stats = map.stats.clone();
        (plan(map, &previous, g), stats)
    });
    let (plan, mut stats) = plan;
    stats.commit = commit;
    stats.branch = branch;
    stats.manifests = tree.files.keys().filter(|p| !p.ends_with("tsconfig.json")).count();
    stats.codeowners = owners_file.map(|(p, _, _)| p);
    notes.sort();
    notes.dedup();
    report.upserted = plan.upserted;
    report.unchanged = plan.unchanged;
    report.deleted = plan.deleted;
    report.links_removed = plan.links_removed;
    report.notes = notes;
    let ctx =
        ActionContext::new(connector_principal(&spec.name), ws.clone()).with_source(format!("connector:{}", spec.name));
    let wrote = !plan.mutations.is_empty();
    apply_batches(onto, &ctx, plan.mutations)?;
    if wrote {
        report.ranked = onto.compute_authority(&ctx, &[], &Weights::default())?.ranked;
    }
    report.ms = (now_ms() - started).max(0) as u64;
    report.repo = Some(stats.clone());
    if wrote {
        onto.apply(
            &ctx,
            vec![Mutation::Event(EventInput {
                action: "connector.crawled".into(),
                target: Some(report.source.clone()),
                payload: json!({
                    "upserted": report.upserted, "unchanged": report.unchanged, "deleted": report.deleted,
                    "links_removed": report.links_removed, "packages": stats.packages,
                    "dependencies": stats.dependencies, "commit": stats.commit, "ms": report.ms,
                }),
                ..Default::default()
            })],
        )?;
    }
    state.fingerprints = plan.fingerprints;
    state.stamp = Some(stamp);
    state.last_crawl_at = started;
    state.last_success_at = now_ms();
    state.last_error = None;
    state.crawls += 1;
    state.last = Some(report.clone());
    save_state(onto, &spec.name, &state)?;
    Ok(report)
}

/// The spec of a repository connector as stored: the URL trimmed and checked.
pub fn validate(spec: &ConnectorSpec) -> Result<()> {
    let url = spec.url.trim();
    if looks_like_url(url) {
        parse_git_url(url)?;
    } else if !Path::new(url).is_absolute() {
        return Err(Error::invalid(format!(
            "repository path '{url}' must be absolute (the CLI resolves relative paths), or a git URL"
        )));
    }
    if let Some(r) = &spec.repo {
        r.validate()?;
    }
    Ok(())
}

/// A connector name from a repository path or URL (`…/platform.git` → `platform`).
pub fn name_from(source: &str) -> String {
    let s = source.trim().trim_end_matches('/');
    let last = s.rsplit(['/', ':']).next().unwrap_or(s);
    let last = last.strip_suffix(".git").unwrap_or(last);
    let name: String = last
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .take(40)
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() { "repo".into() } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_urls() {
        for ok in [
            "https://github.com/acme/platform",
            "https://github.com/acme/platform.git",
            "https://git.example.com:8443/team/repo.git",
            "ssh://git@github.com/acme/platform.git",
            "ssh://github.com:2222/acme/platform",
            "git@github.com:acme/platform.git",
            "deploy.bot@git.example.com:/srv/repo.git",
            "https://[2606:4700::1111]/x/y",
        ] {
            assert!(parse_git_url(ok).is_ok(), "{ok}: {:?}", parse_git_url(ok));
            assert!(looks_like_url(ok), "{ok}");
        }
        for bad in [
            "",
            "ext::sh -c touch% /tmp/pwned",
            "file:///etc",
            "http://github.com/acme/platform",
            "git://github.com/acme/platform",
            "--upload-pack=touch /tmp/x",
            "-c core.sshCommand=x",
            "https://user:token@github.com/acme/platform",
            "https://user@github.com/acme/platform",
            "ssh://u:pw@host/repo",
            "ssh://-oProxyCommand=x/repo",
            "git@-oProxyCommand=x:repo",
            "git@host:-repo",
            "https://github.com",
            "https://github.com/a b",
            "https://github.com/acme/platform?x=1",
            "https://host:0/repo",
            "fd::17",
        ] {
            assert!(parse_git_url(bad).is_err(), "{bad} should be refused");
        }
        assert_eq!(parse_git_url("git@GitHub.com:a/b").unwrap().host, "github.com");
        assert_eq!(parse_git_url("ssh://h:22/r").unwrap().port, Some(22));
        assert!(!looks_like_url("/srv/repo") && !looks_like_url("./repo") && !looks_like_url("C:\\repo"));
    }

    #[test]
    fn restricted_git_urls_are_pinned_without_redirects() {
        let url = |s: &str| parse_git_url(s).unwrap();
        let literal = restricted_reach(&url("https://8.8.8.8/acme/platform"), false).unwrap();
        assert_eq!(literal.config, ["http.followRedirects=false"]);
        for bad in ["https://127.0.0.1/r", "https://10.0.0.7/r", "https://localhost/r", "git@github.com:acme/p.git"] {
            assert!(restricted_reach(&url(bad), false).is_err(), "{bad}");
        }
        // With private URLs allowed: no address check, still no redirects.
        let open = restricted_reach(&url("git@git.internal:acme/p.git"), true).unwrap();
        assert_eq!(open.config, ["http.followRedirects=false"]);
        assert!(restricted_reach(&url("https://127.0.0.1/r"), true).is_ok());
    }

    #[test]
    fn one_clone_per_url_and_branch_and_one_crawl_per_clone() {
        let u = "https://github.com/acme/platform";
        assert_ne!(clone_dir(u, None), clone_dir(u, Some("dev")));
        assert_ne!(clone_dir(u, Some("dev")), clone_dir(u, Some("main")));
        assert_eq!(clone_dir(u, None), clone_dir(u, None));
        // A second claim on a directory waits until the first is dropped.
        let base = std::env::temp_dir().join(new_id("onto-lock"));
        let dir = base.join("clone");
        let first = DirLock::acquire(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let d = dir.clone();
        let waiter = std::thread::spawn(move || {
            let _second = DirLock::acquire(&d).unwrap();
            tx.send(()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "the second claim did not wait");
        drop(first);
        rx.recv_timeout(Duration::from_secs(10)).expect("the second claim never got the directory");
        waiter.join().unwrap();
        // Another directory is independent.
        let _a = DirLock::acquire(&dir).unwrap();
        let _b = DirLock::acquire(&base.join("other")).unwrap();
        drop((_a, _b));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn branch_names() {
        for ok in ["main", "release/1.2", "feat/x_y-z"] {
            assert!(check_branch(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "--upload-pack=x", "a..b", "a b", "x@{1}", "/abs"] {
            assert!(check_branch(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn gitignore_style_patterns() {
        assert!(path_glob("crates/*", "crates/core"));
        assert!(!path_glob("crates/*", "crates/core/sub"));
        assert!(path_glob("packages/**", "packages/a/b"));
        assert!(path_glob("packages/**", "packages"));
        assert!(path_glob("./apps/*", "apps/web"));
        assert!(path_glob("lib-[ab]", "lib-a") && !path_glob("lib-[!ab]", "lib-a"));
        assert!(path_glob("svc-?", "svc-1") && !path_glob("svc-?", "svc-10"));
        // Unanchored names match at any depth; a match covers what is inside.
        assert!(covers("docs", "docs") && covers("docs", "a/docs/x") && !covers("docs", "documents"));
        assert!(covers("/services/", "services/payments"));
        assert!(!covers("/services/", "x/services"));
        assert!(
            covers("services/payments/", "services/payments") && !covers("services/payments/", "x/services/payments")
        );
        assert!(
            covers("**/web", "apps/web") && covers("apps/**/api", "apps/a/b/api") && covers("apps/**/api", "apps/api")
        );
        assert!(covers("*", "") && covers("*", "a/b") && covers("/", "") && covers("/**", "x"));
        assert!(!covers("*.js", "") && !covers("*.js", "web") && covers("*.js", "x.js"));
        assert!(!covers("docs/*", ""));
    }

    #[test]
    fn codeowners_last_rule_wins() {
        let rules = parse_codeowners(
            "# comment\n* @acme/platform\n\n[Docs]\n/services/ @acme/backend dev@acme.com # trailing\n\
             /services/payments/ @acme/payments @ada\n/services/payments/legacy/\nweb\\ app/ @acme/web\n",
        );
        assert_eq!(rules.len(), 5);
        assert_eq!(owners_of(&rules, ""), ["@acme/platform"]);
        assert_eq!(owners_of(&rules, "crates/core"), ["@acme/platform"]);
        assert_eq!(owners_of(&rules, "services/search"), ["@acme/backend", "dev@acme.com"]);
        assert_eq!(owners_of(&rules, "services/payments/api"), ["@acme/payments", "@ada"]);
        assert!(owners_of(&rules, "services/payments/legacy").is_empty(), "a rule without owners unowns");
        assert_eq!(owners_of(&rules, "web app"), ["@acme/web"]);
        let map: BTreeMap<String, String> = [("@ACME/Payments".to_string(), "team:payments".to_string())].into();
        assert_eq!(owner_id("@acme/payments", &map).as_deref(), Some("team:payments"));
        assert_eq!(owner_id("@acme/Back.End", &BTreeMap::new()).as_deref(), Some("team:acme-back-end"));
        assert_eq!(owner_id("@Ada", &BTreeMap::new()).as_deref(), Some("person:gh-ada"));
        assert_eq!(owner_id("dev.ops@acme.com", &BTreeMap::new()).as_deref(), Some("person:dev.ops"));
        assert_eq!(owner_id("@acme/", &BTreeMap::new()), None);
    }

    #[test]
    fn names_and_ids() {
        assert_eq!(reduce_name("@acme/ui-kit"), "acme.ui-kit");
        assert_eq!(reduce_name("github.com/acme/mono/svc"), "github.com.acme.mono.svc");
        assert_eq!(reduce_name("Weird Name+"), "weird-name-");
        assert_eq!(package_id("mono", Ecosystem::Npm, "@acme/web"), "package:mono.npm.acme.web");
        assert_eq!(pep503("Foo__Bar.baz"), "foo-bar-baz");
        assert_eq!(pep508_name("Acme_Core[extra]>=1.0; python_version>'3.9'").as_deref(), Some("acme-core"));
        assert_eq!(name_from("https://github.com/acme/platform.git"), "platform");
        assert_eq!(name_from("git@github.com:acme/Mono-Repo.git"), "mono-repo");
        assert_eq!(name_from("/srv/code/platform/"), "platform");
        assert_eq!(join_rel("crates/a", "../b"), Some("crates/b".into()));
        assert_eq!(join_rel("", "../x"), None);
    }

    #[test]
    fn pnpm_workspace_lists() {
        let text =
            "# root\npackages:\n  - 'packages/*'\n  - \"apps/**\" # apps\n  - '!**/test/**'\ncatalog:\n  react: ^18\n";
        assert_eq!(pnpm_packages(text), ["packages/*", "apps/**", "!**/test/**"]);
        assert_eq!(pnpm_packages("packages: ['a/*', \"b\"]\n"), ["a/*", "b"]);
        assert!(pnpm_packages("catalog:\n  - x\n").is_empty());
    }

    #[test]
    fn go_directive_blocks() {
        let d = go_directives(
            "module example.com/mono/api // the API\n\ngo 1.22\n\nrequire (\n\texample.com/mono/lib v0.0.0\n\tgithub.com/x/y v1.2.3 // indirect\n)\nreplace example.com/mono/lib => ../lib\n",
        );
        assert_eq!(d[0], ("module".into(), "example.com/mono/api".into()));
        assert!(d.contains(&("require".into(), "example.com/mono/lib v0.0.0".into())));
        assert!(d.contains(&("require".into(), "github.com/x/y v1.2.3".into())));
        assert!(d.contains(&("replace".into(), "example.com/mono/lib => ../lib".into())));
    }
}
