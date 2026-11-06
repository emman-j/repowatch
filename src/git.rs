//! Read-only git access.
//!
//! This is the ONLY place that talks to git, and every invocation goes through
//! `run()`, which refuses anything that is not on a small allowlist of
//! inspection commands. There is deliberately no code path for fetch, pull,
//! push, merge, rebase, reset, checkout, clean, commit, branch -d, stash
//! push/drop, gc, etc.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// (subcommand, required second arg if any)
const ALLOWED: &[(&str, Option<&str>)] = &[
    ("status", None),
    ("log", None),
    ("rev-parse", None),
    ("stash", Some("list")),
];

#[derive(Clone, Default, Debug)]
pub struct RepoInfo {
    pub path: PathBuf,
    pub name: String,
    pub branch: String,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: u32,
    pub modified: u32,
    pub untracked: u32,
    pub conflicted: u32,
    pub stashes: u32,
    pub last_commit_ts: Option<i64>,
    pub last_commit_msg: String,
    /// "XY path" lines, capped.
    pub changes: Vec<String>,
    /// "hash  age  author  subject" lines.
    pub recent: Vec<String>,
    pub error: Option<String>,
    /// (remote name, URL with any embedded credentials removed)
    pub remotes: Vec<(String, String)>,
}

impl RepoInfo {
    pub fn is_dirty(&self) -> bool {
        self.staged + self.modified + self.untracked + self.conflicted > 0
    }

    /// The remote to show in the table: the upstream's remote, else "origin", else the first.
    pub fn main_remote(&self) -> Option<&(String, String)> {
        let up = self.upstream.as_deref().and_then(|u| u.split('/').next());
        up.and_then(|n| self.remotes.iter().find(|r| r.0 == n))
            .or_else(|| self.remotes.iter().find(|r| r.0 == "origin"))
            .or_else(|| self.remotes.first())
    }
}

fn guard(args: &[&str]) -> bool {
    // `git remote` can add/remove/rename remotes, so only the exact listing form is allowed.
    if args.first() == Some(&"remote") {
        return args == ["remote", "-v"];
    }
    match args.first() {
        None => false,
        Some(cmd) => ALLOWED.iter().any(|(c, sub)| {
            c == cmd
                && match sub {
                    None => true,
                    Some(s) => args.get(1) == Some(s),
                }
        }),
    }
}

fn run(repo: &Path, args: &[&str]) -> Result<String, String> {
    if !guard(args) {
        // Programmer error: refuse rather than execute.
        return Err(format!("blocked non-read-only git command: {:?}", args));
    }
    let mut cmd = Command::new("git");
    cmd.arg("--no-pager")
        // Never let a repo's config spawn a filesystem-monitor helper.
        .args(["-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0") // `status` must not write/lock the index
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .map_err(|e| format!("could not run git ({e}) - is git installed and on PATH?"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn inspect(path: &Path) -> RepoInfo {
    let mut info = RepoInfo {
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        ..Default::default()
    };

    if !path.exists() {
        info.error = Some("folder not found (moved or on a disconnected drive?)".into());
        return info;
    }

    match run(path, &["status", "--porcelain=v2", "--branch"]) {
        Ok(out) => parse_status(&out, &mut info),
        Err(e) => {
            info.error = Some(if e.is_empty() { "git status failed".into() } else { e });
            return info;
        }
    }

    if let Ok(out) = run(path, &["remote", "-v"]) {
        info.remotes = parse_remotes(&out);
    }

    if let Ok(out) = run(path, &["stash", "list"]) {
        info.stashes = out.lines().filter(|l| !l.is_empty()).count() as u32;
    }

    if let Ok(out) = run(path, &["log", "-n", "15", "--format=%h%x09%ct%x09%an%x09%s"]) {
        for (i, line) in out.lines().enumerate() {
            let p: Vec<&str> = line.splitn(4, '\t').collect();
            if p.len() == 4 {
                let ts: i64 = p[1].parse().unwrap_or(0);
                if i == 0 {
                    info.last_commit_ts = Some(ts);
                    info.last_commit_msg = p[3].to_string();
                }
                info.recent
                    .push(format!("{}  {:>9}  {}  {}", p[0], ago(ts), p[2], p[3]));
            }
        }
    }
    info
}

pub fn parse_status(out: &str, info: &mut RepoInfo) {
    const MAX_CHANGES: usize = 300;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            if rest == "(detached)" {
                info.detached = true;
                info.branch = "(detached HEAD)".into();
            } else {
                info.branch = rest.to_string();
            }
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            info.upstream = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            for tok in rest.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    info.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    info.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let xy: Vec<char> = line[2..].chars().take(2).collect();
            if xy.len() == 2 {
                if xy[0] != '.' {
                    info.staged += 1;
                }
                if xy[1] != '.' {
                    info.modified += 1;
                }
                let fields = if line.starts_with("1 ") { 9 } else { 10 };
                let path = line.splitn(fields, ' ').last().unwrap_or("");
                let path = path.split('\t').next().unwrap_or(path);
                if info.changes.len() < MAX_CHANGES {
                    info.changes
                        .push(format!("{}{}  {}", xy[0], xy[1], path));
                }
            }
        } else if line.starts_with("u ") {
            info.conflicted += 1;
            let path = line.splitn(11, ' ').last().unwrap_or("");
            if info.changes.len() < MAX_CHANGES {
                info.changes.push(format!("UU  {}", path));
            }
        } else if let Some(p) = line.strip_prefix("? ") {
            info.untracked += 1;
            if info.changes.len() < MAX_CHANGES {
                info.changes.push(format!("??  {}", p));
            }
        }
    }
}

/// Parse `git remote -v` output, keeping one (fetch) entry per remote.
pub fn parse_remotes(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let l = l.trim_end().strip_suffix("(fetch)")?.trim_end();
            let (name, url) = l.split_once(char::is_whitespace)?;
            Some((name.to_string(), redact_url(url.trim())))
        })
        .collect()
}

/// Remove credentials from a remote URL before it is ever shown or copied.
/// https://user:token@host/x -> https://host/x ; ssh://u:pw@host -> ssh://u@host
pub fn redact_url(u: &str) -> String {
    let Some((scheme, rest)) = u.split_once("://") else {
        return u.to_string(); // scp-like (git@host:path) or a local path
    };
    let end = rest.find('/').unwrap_or(rest.len());
    let (auth, tail) = rest.split_at(end);
    let Some(at) = auth.rfind('@') else {
        return u.to_string();
    };
    let (userinfo, host) = (&auth[..at], &auth[at + 1..]);
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        format!("{scheme}://{host}{tail}") // user names can be tokens too: drop all of it
    } else {
        let user = userinfo.split(':').next().unwrap_or("");
        format!("{scheme}://{user}@{host}{tail}")
    }
}

/// A browsable https link for a remote, when one can be derived (GitHub/GitLab style).
pub fn web_url(url: &str) -> Option<String> {
    let valid_host = |h: &str| !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    let clean = |p: &str| p.trim_matches('/').trim_end_matches(".git").to_string();
    if let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) {
        let scheme = if url.starts_with("https") { "https" } else { "http" };
        let (host, path) = rest.split_once('/')?;
        let hostname = host.split(':').next()?;
        return (valid_host(hostname) && !clean(path).is_empty())
            .then(|| format!("{scheme}://{host}/{}", clean(path)));
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.rsplit('@').next()?;
        let (host, path) = rest.split_once('/')?;
        let hostname = host.split(':').next()?;
        return (valid_host(hostname) && !clean(path).is_empty())
            .then(|| format!("https://{hostname}/{}", clean(path)));
    }
    if !url.contains("://") {
        // scp-like: git@github.com:user/repo.git
        let rest = url.rsplit('@').next()?;
        let (host, path) = rest.split_once(':')?;
        // host.len() > 1 and no backslashes: don't mistake a Windows path like C:\x for host:path
        let looks_local = host.len() < 2 || path.contains('\\');
        return (!looks_local && valid_host(host) && !clean(path).is_empty())
            .then(|| format!("https://{host}/{}", clean(path)));
    }
    None
}

/// Compact display form: "github.com/user/repo" (or the raw URL if it isn't web-like).
pub fn short_remote(url: &str) -> String {
    match web_url(url) {
        Some(w) => w
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string(),
        None => url.to_string(),
    }
}

pub fn ago(ts: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let d = (now - ts).max(0);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        86400..=2_591_999 => format!("{}d ago", d / 86400),
        2_592_000..=31_535_999 => format!("{}mo ago", d / 2_592_000),
        _ => format!("{}y ago", d / 31_536_000),
    }
}

/// Look for repositories under `root` (read-only directory walk).
pub fn scan(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(root, 0, max_depth, &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, depth: usize, max: usize, out: &mut Vec<PathBuf>) {
    if dir.join(".git").exists() {
        out.push(dir.to_path_buf());
        return; // don't descend into repos (submodules/nested repos ignored)
    }
    if depth >= max {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !p.is_dir() || name.starts_with('.') && name != ".git" {
            continue;
        }
        if matches!(name.as_str(), "node_modules" | "target" | "venv" | ".venv" | "__pycache__") {
            continue;
        }
        // skip symlinks to avoid loops
        if e.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
        }
        walk(&p, depth + 1, max, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_blocks_everything_dangerous() {
        for bad in [
            &["push"][..],
            &["pull"],
            &["fetch"],
            &["merge", "x"],
            &["reset", "--hard"],
            &["checkout", "."],
            &["clean", "-fdx"],
            &["commit", "-am", "x"],
            &["rebase"],
            &["stash"],
            &["stash", "drop"],
            &["stash", "pop"],
            &["branch", "-D", "x"],
            &["gc"],
            &["remote"],
            &["remote", "add", "x", "y"],
            &["remote", "remove", "origin"],
            &["remote", "set-url", "origin", "z"],
            &["remote", "-v", "add", "x", "y"],
            &["remote", "prune", "origin"],
            &["config", "--unset", "remote.origin.url"],
            &[],
        ] {
            assert!(!guard(bad), "should be blocked: {:?}", bad);
        }
    }

    #[test]
    fn guard_allows_inspection() {
        assert!(guard(&["status", "--porcelain=v2", "--branch"]));
        assert!(guard(&["log", "-n", "1"]));
        assert!(guard(&["stash", "list"]));
        assert!(guard(&["rev-parse", "HEAD"]));
        assert!(guard(&["remote", "-v"]));
    }

    #[test]
    fn redacts_credentials() {
        assert_eq!(redact_url("https://user:ghp_secret@github.com/a/b.git"), "https://github.com/a/b.git");
        assert_eq!(redact_url("https://ghp_tokenonly@github.com/a/b"), "https://github.com/a/b");
        assert_eq!(redact_url("ssh://git:pw@host.com:2222/a/b"), "ssh://git@host.com:2222/a/b");
        assert_eq!(redact_url("git@github.com:a/b.git"), "git@github.com:a/b.git");
        assert_eq!(redact_url("/srv/git/x.git"), "/srv/git/x.git");
        let r = parse_remotes("origin\thttps://u:tok@github.com/a/b.git (fetch)\norigin\thttps://u:tok@github.com/a/b.git (push)\n");
        assert_eq!(r, vec![("origin".to_string(), "https://github.com/a/b.git".to_string())]);
    }

    #[test]
    fn derives_web_links() {
        assert_eq!(web_url("git@github.com:a/b.git").as_deref(), Some("https://github.com/a/b"));
        assert_eq!(web_url("https://gitlab.com/g/s/p.git").as_deref(), Some("https://gitlab.com/g/s/p"));
        assert_eq!(web_url("ssh://git@host.com:2222/a/b.git").as_deref(), Some("https://host.com/a/b"));
        assert_eq!(web_url("/srv/git/x.git"), None);
        assert_eq!(web_url("C:\\repos\\x.git"), None);
        assert_eq!(short_remote("git@github.com:a/b.git"), "github.com/a/b");
    }

    #[test]
    fn parses_status() {
        let s = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n\
1 M. N... 100644 100644 100644 a b src/a.rs\n\
1 .M N... 100644 100644 100644 a b src/b.rs\n\
2 R. N... 100644 100644 100644 a b R100 new.rs\told.rs\n\
u UU N... 1 2 3 4 a b c conflict.rs\n\
? new file.txt\n";
        let mut i = RepoInfo::default();
        parse_status(s, &mut i);
        assert_eq!(i.branch, "main");
        assert_eq!((i.ahead, i.behind), (2, 1));
        assert_eq!(i.staged, 2);
        assert_eq!(i.modified, 1);
        assert_eq!(i.conflicted, 1);
        assert_eq!(i.untracked, 1);
        assert!(i.changes.iter().any(|c| c.ends_with("new.rs")));
        assert!(i.changes.iter().any(|c| c.ends_with("new file.txt")));
    }

    #[test]
    fn inspects_real_repo_without_modifying_it() {
        let dir = std::env::temp_dir().join(format!("repowatch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| {
            let ok = Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success();
            assert!(ok, "setup git failed: {:?}", args);
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "T"]);
        g(&["remote", "add", "origin", "https://me:secrettoken@github.com/a/b.git"]);
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        g(&["add", "."]);
        g(&["commit", "-q", "-m", "first"]);
        std::fs::write(dir.join("a.txt"), "2").unwrap();
        std::fs::write(dir.join("new.txt"), "x").unwrap();

        let before = std::fs::read(dir.join(".git/index")).unwrap();
        let info = inspect(&dir);
        let after = std::fs::read(dir.join(".git/index")).unwrap();

        assert!(info.error.is_none(), "{:?}", info.error);
        assert_eq!(info.modified, 1);
        assert_eq!(info.untracked, 1);
        assert_eq!(info.last_commit_msg, "first");
        assert_eq!(info.recent.len(), 1);
        let (rn, ru) = info.main_remote().expect("remote").clone();
        assert_eq!(rn, "origin");
        assert_eq!(ru, "https://github.com/a/b.git");
        assert!(!format!("{:?}", info.remotes).contains("secrettoken"));
        assert_eq!(before, after, "index must not change");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
