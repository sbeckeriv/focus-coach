//! `focus autostart on|off`: a launchd agent that starts the daemon at login.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use focus_core::Paths;

const LABEL: &str = "dev.focuscoach.daemon";

fn plist_path() -> Result<PathBuf> {
    Ok(dirs_home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).context("HOME isn't set")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The agent: runs `focus daemon` at login, with this shell's PATH so `gh` and
/// `claude` are found, and FOCUS_HOME when it isn't the default.
fn plist(exe: &Path, paths: &Paths, path_var: &str, custom_home: bool) -> String {
    let log = escape(&paths.daemon_log().display().to_string());
    let mut env = format!("    <key>PATH</key><string>{}</string>\n", escape(path_var));
    if custom_home {
        env.push_str(&format!("    <key>FOCUS_HOME</key><string>{}</string>\n", escape(&paths.home.display().to_string())));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        escape(&exe.display().to_string()),
    )
}

fn launchctl(args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("launchctl").args(args).output().context("running launchctl")?;
    if !out.status.success() {
        bail!("launchctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn domain() -> Result<String> {
    let out = std::process::Command::new("id").arg("-u").output().context("running id -u")?;
    Ok(format!("gui/{}", String::from_utf8_lossy(&out.stdout).trim()))
}

pub fn run(paths: &Paths, state: Option<&str>) -> Result<()> {
    let path = plist_path()?;
    match state {
        None | Some("status") => {
            match path.exists() {
                true => println!("On: the daemon starts at login ({}).", path.display()),
                false => println!("Off. Turn it on with `focus autostart on`."),
            }
            Ok(())
        }
        Some("on") => {
            let exe = std::env::current_exe()?.canonicalize()?;
            let custom_home = std::env::var_os("FOCUS_HOME").is_some();
            let path_var = std::env::var("PATH").unwrap_or_default();
            std::fs::create_dir_all(path.parent().expect("has a parent"))?;
            std::fs::write(&path, plist(&exe, paths, &path_var, custom_home))?;
            // Replaces an older copy; ignore "not loaded".
            let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain()?)]);
            launchctl(&["bootstrap", &domain()?, &path.display().to_string()])?;
            println!("On: `focus daemon` starts at login ({}).", path.display());
            println!("It runs {}; run this again after moving or reinstalling focus.", exe.display());
            Ok(())
        }
        Some("off") => {
            if !path.exists() {
                println!("Already off.");
                return Ok(());
            }
            // Unloading also stops the daemon it started; the next `focus` command starts one again.
            let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain()?)]);
            std::fs::remove_file(&path)?;
            println!("Off: removed {}.", path.display());
            Ok(())
        }
        Some(other) => bail!("autostart {other}? Use on, off or status"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_daemon_with_this_path_and_home() {
        let paths = Paths { home: "/Users/me/.focus".into() };
        let text = plist(Path::new("/Users/me/.cargo/bin/focus"), &paths, "/usr/bin:/opt/a&b", false);
        assert!(text.contains("<string>/Users/me/.cargo/bin/focus</string><string>daemon</string>"), "{text}");
        assert!(text.contains("<key>PATH</key><string>/usr/bin:/opt/a&amp;b</string>"), "escaped: {text}");
        assert!(text.contains("<key>RunAtLoad</key><true/>"));
        assert!(!text.contains("FOCUS_HOME"), "the default home needs no override");
        assert!(text.contains("/Users/me/.focus/daemon.log"));
        assert!(plist(Path::new("/f"), &paths, "", true).contains("<key>FOCUS_HOME</key><string>/Users/me/.focus</string>"));
    }
}
