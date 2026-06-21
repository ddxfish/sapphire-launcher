// Platform seam: all OS divergence (paths, exec, browser/file open, process kill).
#[cfg(windows)] use crate::CREATE_NO_WINDOW;
use std::path::PathBuf;
use std::process::Command;

// ── Platform seam ───────────────────────────────────────────────────────────
// All OS divergence lives here. Callers stay OS-agnostic. Windows branches
// preserve existing behavior byte-for-byte; the Linux branches are the new path.
// (find_git / find_conda / find_conda_pip live further down, also #[cfg]-split.)

/// Base config dir, shared with Sapphire. Mirrors core/setup.py::get_config_dir():
/// Windows %APPDATA%\Sapphire, Linux $XDG_CONFIG_HOME/sapphire or ~/.config/sapphire.
pub(crate) fn app_config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join("Sapphire");
            }
        }
        PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()))
            .join("AppData").join("Roaming").join("Sapphire")
    }
    #[cfg(not(windows))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("sapphire");
            }
        }
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".config").join("sapphire")
    }
}

/// Default Sapphire install location: ~/sapphire (home folder, not root).
pub(crate) fn default_install_path() -> String {
    #[cfg(windows)]
    {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| "C:\\".to_string());
        format!("{}\\sapphire", home)
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        format!("{}/sapphire", home)
    }
}

/// Resolve how to run Sapphire's Python: the env's interpreter directly if we can
/// find it, else fall back to `conda run`. Returns (program, args-before-script).
pub(crate) fn sapphire_python_cmd() -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let cands = [
            format!("{}\\miniconda3\\envs\\sapphire\\python.exe", home),
            format!("{}\\Miniconda3\\envs\\sapphire\\python.exe", home),
            format!("{}\\anaconda3\\envs\\sapphire\\python.exe", home),
            format!("{}\\Anaconda3\\envs\\sapphire\\python.exe", home),
        ];
        for p in &cands {
            if PathBuf::from(p).exists() { return (p.clone(), vec![]); }
        }
        ("conda".to_string(), vec!["run".into(), "-n".into(), "sapphire".into(), "python".into()])
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        let cands = [
            format!("{}/miniconda3/envs/sapphire/bin/python", home),
            format!("{}/anaconda3/envs/sapphire/bin/python", home),
        ];
        for p in &cands {
            if PathBuf::from(p).exists() { return (p.clone(), vec![]); }
        }
        ("conda".to_string(), vec!["run".into(), "-n".into(), "sapphire".into(), "python".into()])
    }
}

/// Open a URL in the user's default browser.
pub(crate) fn open_url(url: &str) {
    #[cfg(windows)]
    { let _ = hidden_cmd("cmd").args(["/c", "start", url]).spawn(); }
    #[cfg(not(windows))]
    { let _ = hidden_cmd("xdg-open").arg(url).spawn(); }
}

/// Open a file in the user's default viewer/editor.
pub(crate) fn open_file(path: &std::path::Path) {
    #[cfg(windows)]
    { let _ = hidden_cmd("notepad").arg(path).spawn(); }
    #[cfg(not(windows))]
    { let _ = hidden_cmd("xdg-open").arg(path).spawn(); }
}

/// Platform null device for discarding command output.
pub(crate) fn null_device() -> &'static str {
    #[cfg(windows)]
    { "NUL" }
    #[cfg(not(windows))]
    { "/dev/null" }
}

/// Kill Sapphire and its children. Windows: taskkill the tree + sweep env pythons.
/// Linux: kill the process group (we spawn Sapphire in its own group) + sweep.
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        if pid > 0 {
            let _ = hidden_cmd("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).output();
        }
        let home = std::env::var("USERPROFILE").unwrap_or_default();
        let env_path = format!("{}\\miniconda3\\envs\\sapphire", home);
        if let Ok(output) = hidden_cmd("wmic")
            .args(["process", "where", &format!("name='python.exe' and executablepath like '%{}%'", env_path.replace('\\', "\\\\")),
                "get", "processid"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if let Ok(p) = line.trim().parse::<u32>() {
                    let _ = hidden_cmd("taskkill").args(["/F", "/PID", &p.to_string()]).output();
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        if pid > 0 {
            // Negative pid targets the whole process group (Sapphire is spawned
            // into its own group at launch), reaping child STT/TTS workers too.
            let _ = hidden_cmd("kill").args(["-TERM", &format!("-{}", pid)]).output();
        }
        // Sweep any stray python running the sapphire env (e.g. started outside us).
        let _ = hidden_cmd("pkill").args(["-TERM", "-f", "envs/sapphire/bin/python"]).output();
    }
}

/// Create a Command that won't flash a console window on Windows
pub(crate) fn hidden_cmd(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
