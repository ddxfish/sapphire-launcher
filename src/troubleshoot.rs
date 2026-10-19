// Troubleshoot tab: health/dependency/plugin checks and their one-click fixes.
use crate::*;
use crate::platform::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TsCheck {
    SapphireRunning,
    WebUi,
    DepsHealth,
    Plugins,
    PluginDeps,
    Gpu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TsStatus {
    NotChecked,
    Checking,
    Ok(String),
    Problem(String),
    Fixing,
    Fixed(String),
}


// ── Troubleshoot checks ────────────────────────────────────────────────────

pub(crate) async fn ts_check_running(port: u16) -> (TsCheck, TsStatus) {
    match run_cmd_full_async("curl".into(), vec![
        "-sk".into(), "--max-time".into(), "5".into(),
        format!("{}/api/health", web_url(port)),
    ]).await {
        Ok(body) if body.contains("ok") => (TsCheck::SapphireRunning, TsStatus::Ok("Sapphire is responding".into())),
        Ok(_) => (TsCheck::SapphireRunning, TsStatus::Problem("Sapphire responded but health check failed".into())),
        Err(_) => (TsCheck::SapphireRunning, TsStatus::Problem("Can't reach Sapphire — is it running?".into())),
    }
}

pub(crate) async fn ts_check_webui(port: u16) -> (TsCheck, TsStatus) {
    match run_cmd_full_async("curl".into(), vec![
        "-sk".into(), "--max-time".into(), "5".into(),
        "-o".into(), null_device().into(),
        "-w".into(), "%{http_code}".into(),
        "-L".into(), format!("{}/", web_url(port)),
    ]).await {
        Ok(code) => {
            let code = code.trim().to_string();
            // curl output may have extra text before the code — grab last 3 chars
            let status = code.chars().rev().take(3).collect::<String>().chars().rev().collect::<String>();
            if status == "200" {
                (TsCheck::WebUi, TsStatus::Ok("Web UI is loading fine".into()))
            } else if status == "500" {
                (TsCheck::WebUi, TsStatus::Problem("Web UI returns error 500 — likely a package version issue".into()))
            } else {
                (TsCheck::WebUi, TsStatus::Problem(format!("Web UI returned HTTP {}", status)))
            }
        }
        Err(_) => (TsCheck::WebUi, TsStatus::Problem("Can't reach web UI — is Sapphire running?".into())),
    }
}

pub(crate) async fn ts_check_deps() -> (TsCheck, TsStatus) {
    match pip_run(vec!["check".into()]).await {
        Ok(output) if output.contains("No broken requirements") => {
            (TsCheck::DepsHealth, TsStatus::Ok("All dependencies look good".into()))
        }
        Ok(output) => {
            let problems: Vec<&str> = output.lines().take(5).collect();
            (TsCheck::DepsHealth, TsStatus::Problem(format!("Broken packages:\n{}", problems.join("\n"))))
        }
        Err(e) => {
            // pip check returns non-zero when deps are broken — that's the actual output
            if e.contains("has requirement") || e.contains("not installed") {
                let problems: Vec<&str> = e.lines().take(5).collect();
                (TsCheck::DepsHealth, TsStatus::Problem(format!("Broken packages:\n{}", problems.join("\n"))))
            } else {
                (TsCheck::DepsHealth, TsStatus::Problem(format!("Couldn't check: {}", e)))
            }
        }
    }
}


pub(crate) async fn ts_fix_deps(install_path: String) -> (TsCheck, String, bool) {
    let req = PathBuf::from(&install_path).join("requirements.txt");
    match pip_install(vec!["-r".into(), req.to_string_lossy().to_string()]).await {
        Ok(_) => (TsCheck::DepsHealth, "Repaired! Restart Sapphire to apply.".to_string(), true),
        Err(e) => (TsCheck::DepsHealth, format!("Repair failed: {}", e), false),
    }
}

pub(crate) async fn ts_check_gpu() -> (TsCheck, TsStatus) {
    match run_cmd_async("nvidia-smi".into(), vec![
        "--query-gpu=name,memory.total".into(), "--format=csv,noheader,nounits".into(),
    ]).await {
        Ok(output) => {
            let info = output.trim().to_string();
            if let Some((name, mem)) = info.split_once(',') {
                let mem_gb = mem.trim().parse::<f64>().unwrap_or(0.0) / 1024.0;
                (TsCheck::Gpu, TsStatus::Ok(format!("{} ({:.0} GB) — voice features will use GPU", name.trim(), mem_gb)))
            } else {
                (TsCheck::Gpu, TsStatus::Ok(format!("NVIDIA GPU found: {}", info)))
            }
        }
        Err(_) => (TsCheck::Gpu, TsStatus::Ok("No NVIDIA GPU found — voice features will use CPU (still works, just slower)".into())),
    }
}

pub(crate) async fn ts_check_plugins(install_path: String) -> (TsCheck, TsStatus) {
    let install_dir = PathBuf::from(&install_path).join("install");
    if !install_dir.exists() {
        return (TsCheck::Plugins, TsStatus::Ok("No optional features found".into()));
    }

    // Find all requirements-*.txt files
    let mut results: Vec<String> = Vec::new();
    let mut any_missing = false;

    let features = [
        ("requirements-stt.txt", "Speech-to-Text"),
        ("requirements-tts.txt", "Text-to-Speech"),
        ("requirements-wakeword.txt", "Wake Word"),
    ];

    for (file, label) in &features {
        let req_path = install_dir.join(file);
        if !req_path.exists() { continue; }

        // Check each package the file lists (bare names for the summary).
        let specs = read_requirements(&req_path).await;
        let missing: Vec<String> = pip_missing(&specs).await
            .iter().filter_map(|s| requirement_name(s)).collect();

        if missing.is_empty() {
            results.push(format!("{}: installed", label));
        } else {
            any_missing = true;
            results.push(format!("{}: missing {}", label, missing.join(", ")));
        }
    }

    let summary = results.join(" | ");
    if any_missing {
        (TsCheck::Plugins, TsStatus::Problem(summary))
    } else if results.is_empty() {
        (TsCheck::Plugins, TsStatus::Ok("No optional features found".into()))
    } else {
        (TsCheck::Plugins, TsStatus::Ok(summary))
    }
}

pub(crate) async fn ts_fix_plugins(install_path: String) -> (TsCheck, String, bool) {
    let install_dir = PathBuf::from(&install_path).join("install");
    let features = ["requirements-stt.txt", "requirements-tts.txt", "requirements-wakeword.txt"];
    for file in &features {
        let req_path = install_dir.join(file);
        if !req_path.exists() { continue; }
        if let Err(e) = pip_install(vec!["-r".into(), req_path.to_string_lossy().to_string()]).await {
            return (TsCheck::Plugins, format!("Failed on {}: {}", file, e), false);
        }
    }
    (TsCheck::Plugins, "All optional features installed. Restart Sapphire to apply.".into(), true)
}

/// Read enabled Sapphire plugins and their declared pip specifiers. A plugin is enabled
/// if it's listed in `user/webui/plugins.json` `enabled`, OR its manifest sets
/// `default_enabled` and it's not in `disabled` (so we must read every manifest, not just
/// the enabled list — that's how default-on system plugins like memory/email count).
/// Returns (plugin name, [pip specifiers]) for enabled plugins that declare any. Fully
/// graceful: a missing state file, non-plugin folder, or garbled manifest is just skipped.
pub(crate) async fn enabled_plugin_deps(install_path: &str) -> Vec<(String, Vec<String>)> {
    let root = PathBuf::from(install_path);

    let str_list = |v: &serde_json::Value, key: &str| -> Vec<String> {
        v.get(key).and_then(|x| x.as_array())
            .map(|arr| arr.iter().filter_map(|e| e.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    // Enabled/disabled state: prefer the user's web-UI config; fall back to the static
    // default shipped in the repo (the only file present on a fresh clone). Mirrors
    // Sapphire's plugin_loader.py read order, so we don't see empty lists on a clone.
    let state_paths = [
        root.join("user").join("webui").join("plugins.json"),
        root.join("interfaces").join("web").join("static").join("core-ui").join("plugins.json"),
    ];
    let (mut enabled_list, mut disabled_list) = (Vec::new(), Vec::new());
    for sp in &state_paths {
        if let Ok(txt) = tokio::fs::read_to_string(sp).await {
            let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
            enabled_list = str_list(&v, "enabled");
            disabled_list = str_list(&v, "disabled");
            break;
        }
    }

    // Scan BOTH plugin roots Sapphire loads from: system (plugins/) and user
    // (user/plugins/). A user-installed plugin with pip deps lives only in the latter.
    let plugin_dirs = [root.join("plugins"), root.join("user").join("plugins")];
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for dir in &plugin_dirs {
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else { continue; };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(txt) = tokio::fs::read_to_string(entry.path().join("plugin.json")).await else { continue };
            let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
            let name = manifest.get("name").and_then(|n| n.as_str()).map(str::to_string)
                .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
            let default_enabled = manifest.get("default_enabled").and_then(|b| b.as_bool()).unwrap_or(false);
            let is_enabled = enabled_list.iter().any(|e| e == &name)
                || (default_enabled && !disabled_list.iter().any(|d| d == &name));
            if !is_enabled { continue; }
            let specs: Vec<String> = manifest.get("pip_dependencies").and_then(|d| d.as_array())
                .map(|arr| arr.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if !specs.is_empty() {
                out.push((name, specs));
            }
        }
    }
    out
}

/// Troubleshoot check: for each ENABLED plugin, verify its `pip_dependencies` are
/// installed (via `pip show`). Disabled plugins and dep-free plugins are ignored.
pub(crate) async fn ts_check_plugin_deps(install_path: String) -> (TsCheck, TsStatus) {
    let plugins = enabled_plugin_deps(&install_path).await;
    if plugins.is_empty() {
        return (TsCheck::PluginDeps, TsStatus::Ok("No enabled plugins need extra packages".into()));
    }
    let mut report: Vec<String> = Vec::new();
    for (name, specs) in &plugins {
        let missing = pip_missing(specs).await;
        if !missing.is_empty() {
            report.push(format!("{}: missing {}", name, missing.join(", ")));
        }
    }
    if report.is_empty() {
        (TsCheck::PluginDeps, TsStatus::Ok("All enabled plugins have their dependencies".into()))
    } else {
        (TsCheck::PluginDeps, TsStatus::Problem(report.join(" | ")))
    }
}

/// The "Install" button for plugin deps: pip-install every enabled plugin's specifiers
/// in one call (pip skips already-satisfied ones, so we don't need to pre-filter).
pub(crate) async fn ts_fix_plugin_deps(install_path: String) -> (TsCheck, String, bool) {
    let specs: Vec<String> = enabled_plugin_deps(&install_path).await
        .into_iter().flat_map(|(_, s)| s).collect();
    if specs.is_empty() {
        return (TsCheck::PluginDeps, "No plugin dependencies to install.".into(), true);
    }
    match pip_install(specs).await {
        Ok(_) => (TsCheck::PluginDeps, "Plugin dependencies installed. Restart Sapphire to apply.".into(), true),
        Err(e) => (TsCheck::PluginDeps, format!("Install failed: {}", e), false),
    }
}


pub(crate) fn ts_label(check: TsCheck) -> &'static str {
    match check {
        TsCheck::SapphireRunning => "Sapphire responding",
        TsCheck::WebUi => "Web UI loading",
        TsCheck::DepsHealth => "Package health",
        TsCheck::Plugins => "Optional features",
        TsCheck::PluginDeps => "Plugin dependencies",
        TsCheck::Gpu => "GPU",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("sapphire-launcher-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(&r).unwrap();
        r
    }
    fn put(p: PathBuf, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[tokio::test]
    async fn enabled_plugin_deps_follows_core_rules() {
        let r = root("plugins");
        put(r.join("plugins/alpha/plugin.json"), r#"{"name":"alpha","pip_dependencies":["foo>=1"]}"#); // off, not enabled
        put(r.join("plugins/beta/plugin.json"), r#"{"name":"beta","default_enabled":true,"pip_dependencies":["bar"]}"#); // on by default
        put(r.join("plugins/gamma/plugin.json"), r#"{"name":"gamma","default_enabled":true,"pip_dependencies":["baz"]}"#); // default on, disabled
        put(r.join("plugins/eps/plugin.json"), r#"{"name":"eps","default_enabled":true}"#); // no deps
        put(r.join("plugins/broken/plugin.json"), "{nope"); // garbled → skipped
        put(r.join("user/plugins/delta/plugin.json"), r#"{"name":"delta","pip_dependencies":["qux==2"]}"#); // user plugin, enabled
        put(r.join("user/webui/plugins.json"), r#"{"enabled":["delta"],"disabled":["gamma"]}"#);
        let mut got = enabled_plugin_deps(&r.to_string_lossy()).await;
        got.sort();
        assert_eq!(got, vec![
            ("beta".to_string(), vec!["bar".to_string()]),
            ("delta".to_string(), vec!["qux==2".to_string()]),
        ]);
    }

    #[tokio::test]
    async fn enabled_plugin_deps_falls_back_to_the_shipped_state_file() {
        let r = root("plugins-fallback");
        put(r.join("plugins/alpha/plugin.json"), r#"{"name":"alpha","pip_dependencies":["foo"]}"#);
        put(r.join("interfaces/web/static/core-ui/plugins.json"), r#"{"enabled":["alpha"],"disabled":[]}"#);
        assert_eq!(enabled_plugin_deps(&r.to_string_lossy()).await, vec![("alpha".to_string(), vec!["foo".to_string()])]);
        let empty = root("plugins-none");
        assert!(enabled_plugin_deps(&empty.to_string_lossy()).await.is_empty());
    }
}
