//! Reading one skill from a public GitHub repository, on the host, through
//! the REST API (https://docs.github.com/en/rest/repos/contents).
//!
//! The ref is resolved to a commit SHA first, and everything after that is
//! read at that commit, so what is previewed is what is saved. Requests
//! carry no credentials, so only public repositories can be read, and
//! GitHub allows 60 of them an hour per IP address. Redirects are not
//! followed. Every response is read with a byte cap.

use crate::custom::skills::MAX_SKILL_BYTES;
use crate::user_skills::SkillFile;
use anyhow::{Context, Result, bail, ensure};
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use serde::Deserialize;
use std::time::Duration;
use url::Url;

/// GitHub's REST API.
pub const API: &str = "https://api.github.com";
/// The largest file kept, `SKILL.md` included. A larger `SKILL.md` refuses
/// the skill; any other larger file is skipped.
pub const MAX_FILE_BYTES: usize = MAX_SKILL_BYTES;
/// The most files a skill may list besides `SKILL.md`.
pub const MAX_FILES: usize = 32;
/// The most bytes of all of a skill's files together.
pub const MAX_TOTAL_BYTES: usize = 256 * 1024;
/// The most directories read, the skill's own included.
pub const MAX_DIRS: usize = 8;
/// How deep below the skill's directory a file may be.
pub const MAX_DEPTH: usize = 3;
/// The largest directory listing read.
const MAX_LISTING_BYTES: usize = 256 * 1024;
/// How long one request may take.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// Where a skill is: a directory of a repository at a ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub owner: String,
    pub repo: String,
    /// `a/b`, or empty for the repository's root.
    pub path: String,
    /// A branch, tag or commit; `HEAD` is the default branch.
    pub reference: String,
}

/// A name GitHub and a URL path can both carry unambiguously: 1 to 100 of
/// ASCII letters, digits, `.`, `_` and `-`, and not `.` or `..`.
fn safe_name(name: &str) -> bool {
    (1..=100).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && name != "."
        && name != ".."
}

impl Source {
    /// `repo` is `owner/name` or `https://github.com/owner/name`. `path` is
    /// the directory holding `SKILL.md`; `reference` defaults to `HEAD`.
    pub fn parse(repo: &str, path: Option<&str>, reference: Option<&str>) -> Result<Self> {
        let repo = repo.trim();
        let rest = match repo.strip_prefix("https://github.com/") {
            Some(rest) => rest.trim_end_matches('/'),
            None if repo.contains(':') => {
                bail!("`repo` must be on https://github.com; other hosts are not supported")
            }
            None => repo,
        };
        let rest = rest.strip_suffix(".git").unwrap_or(rest);
        let (owner, name) = rest
            .split_once('/')
            .context("`repo` must be owner/name or https://github.com/owner/name")?;
        ensure!(
            safe_name(owner) && safe_name(name),
            "`repo` must be owner/name, each 1 to 100 letters, digits, `.`, `_` or `-`"
        );
        let path = path.unwrap_or_default().trim().trim_matches('/');
        ensure!(
            path.is_empty() || path.split('/').all(safe_name),
            "`path` must be a directory in the repository: names of letters, digits, `.`, \
             `_` or `-` separated by `/`, with no `.` or `..`"
        );
        let reference = reference.map(str::trim).unwrap_or("HEAD");
        ensure!(
            safe_name(reference) && !reference.starts_with(['.', '-']),
            "`ref` must be a branch, tag or commit SHA of letters, digits, `.`, `_` or `-`; \
             for a branch whose name has a `/`, give its commit SHA"
        );
        Ok(Self {
            owner: owner.into(),
            repo: name.into(),
            path: path.into(),
            reference: reference.into(),
        })
    }

    /// `owner/name`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// The directory the skill's `name` must match: the path's last
    /// segment, or the repository's name for a skill at its root.
    pub fn dir_name(&self) -> &str {
        self.path
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.repo)
    }
}

/// What [`GitHub::fetch`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The commit everything was read at.
    pub sha: String,
    pub skill_md: String,
    /// Every other text file, by path relative to the skill's directory.
    pub files: Vec<SkillFile>,
    /// What was left out, and why.
    pub skipped: Vec<String>,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    size: u64,
}

/// The GitHub REST API, or a fake of it in tests.
#[derive(Clone)]
pub struct GitHub {
    base: Url,
    http: reqwest::Client,
}

impl Default for GitHub {
    fn default() -> Self {
        Self::new(Url::parse(API).expect("the API URL parses"), TIMEOUT)
    }
}

/// `dir/name`, where either may be empty.
fn join(dir: &str, name: &str) -> String {
    match (dir, name) {
        ("", name) => name.to_string(),
        (dir, "") => dir.to_string(),
        (dir, name) => format!("{dir}/{name}"),
    }
}

impl GitHub {
    /// The API at `base` (an http or https URL), each request given
    /// `timeout`.
    pub fn new(base: Url, timeout: Duration) -> Self {
        // Only fails if the TLS backend cannot initialise, which is a build
        // problem, not a runtime condition.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent("athena")
            .build()
            .expect("reqwest client builds with the rustls backend");
        Self { base, http }
    }

    /// `<base>/repos/<owner>/<repo>/<parts...>`, each part percent-encoded.
    fn url(&self, source: &Source, parts: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("the API URL is http or https")
            .pop_if_empty()
            .extend(["repos", &source.owner, &source.repo])
            .extend(parts);
        url
    }

    /// The contents API's URL for `path` (relative to the repository) at
    /// `sha`.
    fn contents(&self, source: &Source, path: &str, sha: &str) -> Url {
        let mut parts = vec!["contents"];
        parts.extend(path.split('/'));
        let mut url = self.url(source, &parts);
        url.query_pairs_mut().append_pair("ref", sha);
        url
    }

    /// The body of a successful GET of `url`, at most `cap` bytes.
    async fn get(&self, url: Url, accept: &str, cap: usize) -> Result<Vec<u8>> {
        let mut response = self
            .http
            .get(url)
            .header(ACCEPT, accept)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            bail!(
                "GitHub has no such repository, path or ref; only public repositories can \
                 be installed"
            );
        }
        if status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS {
            bail!(
                "GitHub refused the request ({status}). It allows 60 requests an hour without \
                 an account, so try again later"
            );
        }
        if status.is_redirection() {
            bail!("GitHub says the repository moved ({status}); use its new name");
        }
        ensure!(status.is_success(), "GitHub answered {status}");
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            body.extend_from_slice(&chunk);
            ensure!(body.len() <= cap, "GitHub sent more than {cap} bytes");
        }
        Ok(body)
    }

    /// Resolve `source`'s ref to a commit and read the skill there:
    /// `SKILL.md` and the text files beside and below it.
    pub async fn fetch(&self, source: &Source) -> Result<Fetched> {
        let sha = self
            .get(
                self.url(source, &["commits", &source.reference]),
                "application/vnd.github.sha",
                64,
            )
            .await?;
        let sha = String::from_utf8_lossy(&sha).trim().to_string();
        ensure!(
            sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "GitHub did not answer with a commit SHA"
        );
        let (wanted, mut skipped) = self.list(source, &sha).await?;
        let mut fetched = Fetched {
            sha,
            skill_md: String::new(),
            files: Vec::new(),
            skipped: Vec::new(),
        };
        for path in wanted {
            let url = self.contents(source, &join(&source.path, &path), &fetched.sha);
            let bytes = self
                .get(url, "application/vnd.github.raw", MAX_FILE_BYTES)
                .await?;
            match (path.as_str(), String::from_utf8(bytes)) {
                ("SKILL.md", Ok(text)) => fetched.skill_md = text,
                ("SKILL.md", Err(_)) => bail!("SKILL.md is not UTF-8 text"),
                (_, Ok(content)) => fetched.files.push(SkillFile { path, content }),
                (_, Err(_)) => skipped.push(format!("{path}: not text")),
            }
        }
        let total =
            fetched.skill_md.len() + fetched.files.iter().map(|f| f.content.len()).sum::<usize>();
        ensure!(
            total <= MAX_TOTAL_BYTES,
            "the skill's files are over {MAX_TOTAL_BYTES} bytes together"
        );
        fetched.skipped = skipped;
        Ok(fetched)
    }

    /// The files of the skill to read, `SKILL.md` first, relative to its
    /// directory, and what was skipped. Refuses a skill with no `SKILL.md`
    /// or past a limit.
    async fn list(&self, source: &Source, sha: &str) -> Result<(Vec<String>, Vec<String>)> {
        let mut dirs = vec![(String::new(), 0)];
        let (mut files, mut skipped, mut total) = (Vec::new(), Vec::new(), 0);
        let mut next = 0;
        while let Some((dir, depth)) = dirs.get(next).cloned() {
            next += 1;
            ensure!(
                next <= MAX_DIRS,
                "the skill has more than {MAX_DIRS} directories"
            );
            let url = self.contents(source, &join(&source.path, &dir), sha);
            let listing = self
                .get(url, "application/vnd.github+json", MAX_LISTING_BYTES)
                .await?;
            let entries: Vec<Entry> = serde_json::from_slice(&listing).context(
                "`path` must name a directory of the repository, the one holding SKILL.md",
            )?;
            for entry in entries {
                // A name is the repository's: it is checked before it is
                // used in a path or shown to anyone.
                if !safe_name(&entry.name) {
                    skipped.push("an entry with an unsupported name".into());
                    continue;
                }
                let path = join(&dir, &entry.name);
                match entry.kind.as_str() {
                    "file" if entry.size > MAX_FILE_BYTES as u64 => {
                        ensure!(
                            path != "SKILL.md",
                            "SKILL.md is over {MAX_FILE_BYTES} bytes"
                        );
                        skipped.push(format!("{path}: over {MAX_FILE_BYTES} bytes"));
                    }
                    "file" => {
                        total += entry.size;
                        files.push(path);
                    }
                    "dir" if depth < MAX_DEPTH => dirs.push((path, depth + 1)),
                    "dir" => skipped.push(format!("{path}: nested deeper than {MAX_DEPTH}")),
                    _ => skipped.push(format!("{path}: not a file or directory")),
                }
            }
        }
        let Some(at) = files.iter().position(|f| f == "SKILL.md") else {
            bail!("there is no SKILL.md in that directory");
        };
        ensure!(
            files.len() <= MAX_FILES + 1,
            "the skill has more than {MAX_FILES} files besides SKILL.md"
        );
        ensure!(
            total <= MAX_TOTAL_BYTES as u64,
            "the skill's files are over {MAX_TOTAL_BYTES} bytes together"
        );
        let skill_md = files.remove(at);
        files.insert(0, skill_md);
        Ok((files, skipped))
    }
}
