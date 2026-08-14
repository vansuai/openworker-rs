//! Session environment context — injected into the system prompt at engine build.
//!
//! Mirrors `coworker/environment.py`. Saves the agent 3-4 discovery tool calls every
//! session (pwd, uname, git status, git log) by telling it up front where it is and what
//! state the workspace is in. The git snapshot is point-in-time; the prompt labels it as
//! such so the agent re-checks before relying on it.

use std::time::Duration;

/// Run `git -C <workspace> <args>` with a 5s timeout; None on spawn/timeout/non-zero exit.
async fn git(workspace: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(args)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Mirror of `environment.py::_git_snapshot`.
async fn git_snapshot(workspace: &str) -> Vec<String> {
    if git(workspace, &["rev-parse", "--is-inside-work-tree"])
        .await
        .as_deref()
        != Some("true")
    {
        return vec!["Git: not a git repository".to_string()];
    }

    let mut lines = Vec::new();
    let branch = git(workspace, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(unknown)".to_string());
    lines.push(format!("Git branch: {branch}"));

    if let Some(status) = git(workspace, &["status", "--porcelain"]).await {
        let changed: Vec<&str> = status.lines().collect();
        if changed.is_empty() {
            lines.push("Git status: clean".to_string());
        } else {
            let shown = changed
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let more = if changed.len() > 20 {
                format!("\n\u{2026} and {} more", changed.len() - 20)
            } else {
                String::new()
            };
            lines.push(format!(
                "Git status ({} changed):\n{shown}{more}",
                changed.len()
            ));
        }
    }

    if let Some(log) = git(workspace, &["log", "-n5", "--pretty=format:%h %s"]).await {
        if !log.is_empty() {
            lines.push(format!("Recent commits:\n{log}"));
        }
    }
    lines
}

/// Human OS label, mirroring `platform.mac_ver()` / `platform.system() + release()`.
async fn os_name() -> String {
    if cfg!(target_os = "macos") {
        if let Ok(out) = tokio::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .await
        {
            if out.status.success() {
                let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !version.is_empty() {
                    return format!("macOS {version}");
                }
            }
        }
    }
    if let Ok(out) = tokio::process::Command::new("uname")
        .args(["-s", "-r"])
        .output()
        .await
    {
        if out.status.success() {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    std::env::consts::OS.to_string()
}

/// `sys.platform` token equivalent.
fn platform_token() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// Mirror of `environment.py::environment_context` — a system-prompt block describing the
/// session's environment, git state, and folder scope.
pub async fn environment_context(workspace: &str) -> String {
    // expanduser + resolve, mirroring Path(workspace).expanduser().resolve()
    let expanded = shellexpand::tilde(workspace);
    let ws = std::path::Path::new(expanded.as_ref())
        .canonicalize()
        .unwrap_or_else(|_| std::path::Path::new(expanded.as_ref()).to_path_buf());
    let ws_str = ws.display().to_string();

    let os_name = os_name().await;
    let today = chrono::Local::now().format("%Y-%m-%d");
    let mut lines = vec![
        format!("Workspace: {ws_str}"),
        format!("Platform: {} ({os_name})", platform_token()),
        format!("Session started (date snapshot — prefer per-turn <system-context> for current date): {today}"),
    ];
    lines.extend(git_snapshot(&ws_str).await);
    let body = lines.join("\n");

    let mut text = String::new();
    text.push_str(
        "Environment (snapshot from session start \u{2014} date and git state may be stale; \
         prefer per-turn <system-context>):\n",
    );
    text.push_str("<environment>\n");
    text.push_str(&body);
    text.push_str("\n</environment>\n");
    text.push_str(
        "Folder scope: work inside the workspace and any folders the user has granted. Do not \
         read or list other locations (home directory sweeps, ~/Desktop, ~/Downloads, photo \
         libraries, etc.) \u{2014} not even via shell commands like find/ls/grep. On macOS every \
         such touch fires an OS permission prompt the user can't connect to any action they \
         took. If a task needs files elsewhere, ask first with request_directory.",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ocw-env-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // environment_context canonicalizes the workspace (e.g. /tmp -> /private/tmp on
        // macOS), so compare against the same resolved path.
        dir.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn non_git_workspace_reports_not_a_repo() {
        let dir = temp_dir("plain");
        let text = environment_context(dir.to_str().unwrap()).await;
        assert!(text.contains("Git: not a git repository"), "got: {text}");
        assert!(text.contains("Folder scope:"));
        assert!(text.contains("<environment>"));
        assert!(text.contains(&format!("Workspace: {}", dir.display())));
        assert!(text.contains("Session started"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn git_workspace_reports_branch() {
        let dir = temp_dir("git");
        let init = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .output();
        let Ok(init) = init else {
            eprintln!("git unavailable; skipping");
            return;
        };
        if !init.status.success() {
            eprintln!("git init failed; skipping");
            return;
        }
        // A branch only resolves once HEAD points at a commit (matches Python behavior).
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        for args in [
            vec!["add", "f.txt"],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output();
            if out.map(|o| !o.status.success()).unwrap_or(true) {
                eprintln!("git commit setup failed; skipping");
                return;
            }
        }
        let text = environment_context(dir.to_str().unwrap()).await;
        assert!(text.contains("Git branch: main"), "got: {text}");
        assert!(text.contains("Git status: clean"), "got: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
