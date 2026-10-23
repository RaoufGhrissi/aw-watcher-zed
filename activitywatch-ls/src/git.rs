use std::path::Path;

use tokio::process::Command;

async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .ok()?;
    let stdout = String::from_utf8(output.stdout).ok()?;
    output.status.success().then(|| stdout.trim().to_string())
}

// None on a detached HEAD or outside a repository
pub async fn branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--short", "HEAD"]).await
}

// `owner/name` of the remote the branch tracks, else of `origin`
pub async fn repository(dir: &Path, branch: Option<&str>) -> Option<String> {
    let tracked = match branch {
        Some(branch) => git(dir, &["config", &format!("branch.{branch}.remote")]).await,
        None => None,
    };
    let remote = tracked.unwrap_or_else(|| "origin".to_string());
    repository_name(&git(dir, &["remote", "get-url", &remote]).await?)
}

// `owner/name` for `git@github.com:owner/name.git` or `https://user:token@github.com/owner/name`.
// Only the path is kept, so credentials embedded in the URL are never sent.
fn repository_name(url: &str) -> Option<String> {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?.1,
        // git@host:owner/name
        None => url.split_once(':').map_or(url, |(_, path)| path),
    };
    let path = path.trim_end_matches('/');
    let name = path.strip_suffix(".git").unwrap_or(path);
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_name_of_urls() {
        for (url, expected) in [
            ("git@github.com:owner/name.git", "owner/name"),
            ("https://github.com/owner/name", "owner/name"),
            (
                "https://user:token@github.com/owner/name.git/",
                "owner/name",
            ),
            (
                "ssh://git@gitlab.com:2222/group/sub/project.git",
                "group/sub/project",
            ),
            ("/srv/git/project.git", "/srv/git/project"),
        ] {
            assert_eq!(repository_name(url).as_deref(), Some(expected), "{url}");
        }
    }
}
