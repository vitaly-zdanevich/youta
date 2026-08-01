//! Deterministic build metadata for the offline recent-commits screen.
//!
//! A Git worktree is authoritative when available. Source archives carry an
//! `export-subst` record for their exact commit, while the checked-in snapshot
//! supplies the remaining offline history without adding a build dependency.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const ARCHIVE_METADATA_PATH: &str = ".git_archival.txt";
const CARGO_VCS_METADATA_PATH: &str = ".cargo_vcs_info.json";
const FALLBACK_HISTORY_PATH: &str = "build/recent-commits.tsv";
const GENERATED_METADATA_NAME: &str = "build_info_generated.rs";
const MAX_COMMITS: usize = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Commit {
    hash: String,
    committed_at: String,
    message: String,
}

fn main() {
    let manifest_directory = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo must set CARGO_MANIFEST_DIR"),
    );
    let output_directory = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo must set OUT_DIR"));

    emit_rebuild_inputs(&manifest_directory);

    let fallback = read_fallback_history(&manifest_directory.join(FALLBACK_HISTORY_PATH));
    let archive_commit = read_archive_commit(&manifest_directory.join(ARCHIVE_METADATA_PATH));
    let cargo_vcs_hash = read_cargo_vcs_hash(&manifest_directory.join(CARGO_VCS_METADATA_PATH));
    let git_history = read_git_history(&manifest_directory);
    let git_current_hash = git_history
        .as_ref()
        .and_then(|commits| commits.first())
        .map(|commit| commit.hash.clone());
    let mut commits = git_history.unwrap_or_default();
    if commits.is_empty()
        && let Some(commit) = archive_commit.clone()
    {
        push_unique(&mut commits, commit);
    }
    for commit in fallback {
        push_unique(&mut commits, commit);
    }
    commits.truncate(MAX_COMMITS);

    let current_hash = git_current_hash
        .as_deref()
        .or_else(|| archive_commit.as_ref().map(|commit| commit.hash.as_str()))
        .or(cargo_vcs_hash.as_deref())
        .or_else(|| commits.first().map(|commit| commit.hash.as_str()))
        .unwrap_or("unknown");
    let build_origin = env::var("YOUTA_BUILD_ORIGIN").unwrap_or_else(|_| "local".to_owned());

    let generated =
        generate_rust_source(&commits, current_hash, &build_origin, &manifest_directory);
    fs::write(output_directory.join(GENERATED_METADATA_NAME), generated)
        .expect("write generated build metadata");
}

fn emit_rebuild_inputs(manifest_directory: &Path) {
    println!("cargo:rerun-if-changed={ARCHIVE_METADATA_PATH}");
    println!("cargo:rerun-if-changed={CARGO_VCS_METADATA_PATH}");
    println!("cargo:rerun-if-changed={FALLBACK_HISTORY_PATH}");
    println!("cargo:rerun-if-env-changed=YOUTA_BUILD_ORIGIN");

    let Some(git_directory) = resolve_git_directory(manifest_directory) else {
        return;
    };
    for relative in ["HEAD", "logs/HEAD", "packed-refs"] {
        let path = git_directory.join(relative);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Ok(head) = fs::read_to_string(git_directory.join("HEAD"))
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        let path = git_directory.join(reference);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn resolve_git_directory(manifest_directory: &Path) -> Option<PathBuf> {
    let dot_git = manifest_directory.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let pointer = fs::read_to_string(dot_git).ok()?;
    let path = pointer.trim().strip_prefix("gitdir: ")?;
    let path = PathBuf::from(path);
    Some(if path.is_absolute() {
        path
    } else {
        manifest_directory.join(path)
    })
}

fn read_git_history(manifest_directory: &Path) -> Option<Vec<Commit>> {
    let output = Command::new("git")
        .args(["rev-list", "--max-count=10", "HEAD"])
        .current_dir(manifest_directory)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let hashes = String::from_utf8(output.stdout).ok()?;
    let commits = hashes
        .lines()
        .filter(|hash| valid_hash(hash))
        .filter_map(|hash| read_git_commit(manifest_directory, hash))
        .take(MAX_COMMITS)
        .collect::<Vec<_>>();
    (!commits.is_empty()).then_some(commits)
}

fn read_git_commit(manifest_directory: &Path, hash: &str) -> Option<Commit> {
    let output = Command::new("git")
        .args(["show", "-s", "--format=%H%x00%cI%x00%B%x00", hash])
        .current_dir(manifest_directory)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut fields = output.stdout.splitn(4, |byte| *byte == 0);
    let hash = std::str::from_utf8(fields.next()?).ok()?.trim();
    let committed_at = std::str::from_utf8(fields.next()?).ok()?.trim();
    let message = std::str::from_utf8(fields.next()?).ok()?;
    commit_from_parts(hash, committed_at, message)
}

fn read_archive_commit(path: &Path) -> Option<Commit> {
    let content = fs::read_to_string(path).ok()?;
    let hash = content.lines().next()?.strip_prefix("hash: ")?;
    let remainder = content
        .strip_prefix(content.lines().next()?)?
        .strip_prefix('\n')?;
    let committed_at_line = remainder.lines().next()?;
    let committed_at = committed_at_line.strip_prefix("committed-at: ")?;
    let message_prefix = format!("{committed_at_line}\nmessage:\n");
    let message = remainder.strip_prefix(&message_prefix)?;
    commit_from_parts(hash, committed_at, message)
}

fn read_cargo_vcs_hash(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let sha_key = content.find("\"sha1\"")?;
    let after_key = content.get(sha_key + "\"sha1\"".len()..)?;
    let after_colon = after_key.get(after_key.find(':')? + 1..)?;
    let value = after_colon.trim_start().strip_prefix('"')?;
    let hash = value.get(..value.find('"')?)?;
    valid_hash(hash).then(|| hash.to_owned())
}

fn read_fallback_history(path: &Path) -> Vec<Commit> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let hash = fields.next()?;
            let committed_at = fields.next()?;
            let message = unescape_field(fields.next()?)?;
            commit_from_parts(hash, committed_at, &message)
        })
        .take(MAX_COMMITS)
        .collect()
}

fn commit_from_parts(hash: &str, committed_at: &str, message: &str) -> Option<Commit> {
    if !valid_hash(hash)
        || committed_at.is_empty()
        || committed_at.len() > 128
        || committed_at.chars().any(char::is_control)
    {
        return None;
    }
    let message = message
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim_end_matches('\n')
        .to_owned();
    if message.is_empty() || message.contains('\0') {
        return None;
    }
    Some(Commit {
        hash: hash.to_owned(),
        committed_at: committed_at.to_owned(),
        message,
    })
}

fn valid_hash(hash: &str) -> bool {
    (7..=64).contains(&hash.len()) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn unescape_field(field: &str) -> Option<String> {
    let mut output = String::with_capacity(field.len());
    let mut characters = field.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match characters.next()? {
            '\\' => output.push('\\'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            _ => return None,
        }
    }
    Some(output)
}

fn push_unique(commits: &mut Vec<Commit>, commit: Commit) {
    if !commits.iter().any(|existing| existing.hash == commit.hash) {
        commits.push(commit);
    }
}

fn generate_rust_source(
    commits: &[Commit],
    current_hash: &str,
    build_origin: &str,
    manifest_directory: &Path,
) -> String {
    let mut source = String::from("// @generated by build.rs; do not edit.\n");
    writeln!(
        source,
        "const EMBEDDED_BUILD_ORIGIN: &str = {build_origin:?};"
    )
    .expect("writing to a String cannot fail");
    let build_source_directory = manifest_directory.to_string_lossy();
    writeln!(
        source,
        "const EMBEDDED_BUILD_SOURCE_DIRECTORY: &str = {build_source_directory:?};"
    )
    .expect("writing to a String cannot fail");
    writeln!(
        source,
        "const EMBEDDED_CURRENT_BUILD_SHA: &str = {current_hash:?};"
    )
    .expect("writing to a String cannot fail");
    source.push_str("static EMBEDDED_BUILD_COMMITS: &[BuildCommit] = &[\n");
    for commit in commits.iter().take(MAX_COMMITS) {
        source.push_str("    BuildCommit {\n");
        let hash = &commit.hash;
        let committed_at = &commit.committed_at;
        let message = &commit.message;
        writeln!(source, "        hash: {hash:?},").expect("writing to a String cannot fail");
        writeln!(source, "        committed_at: {committed_at:?},")
            .expect("writing to a String cannot fail");
        writeln!(source, "        message: {message:?},").expect("writing to a String cannot fail");
        source.push_str("    },\n");
    }
    source.push_str("];\n");
    source
}
