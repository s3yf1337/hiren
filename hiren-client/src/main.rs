//! hiren-client — declarative UI runtime launcher.
//!
//! Launcher logic (IPC, state, freq, modes) + UI runtime (scene graph,
//! bindings, layout, animation, rendering). No GTK, no hardcoded UI:
//! the interface is a TOML scene graph resolved against launcher state.

use anyhow::Result;
use hiren_client::config::{self, LauncherConfig};
use hiren_client::frontend::AppCore;
use hiren_client::launcher::{LauncherState, ObservableState};
use hiren_client::ui_runtime::{theme::Theme, UiRuntime};
use hiren_client::{window};
use std::rc::Rc;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|a| a == "--list-themes") {
        for t in config::available_themes() {
            println!("{t}");
        }
        return Ok(());
    }

    let theme_override = arg_value(&args, "--theme");
    let size_override = match (
        arg_value(&args, "--width").and_then(|v| v.parse::<u32>().ok()),
        arg_value(&args, "--height").and_then(|v| v.parse::<u32>().ok()),
    ) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    };
    let reload = args.iter().any(|a| a == "--reload");
    let no_layer_shell = args.iter().any(|a| a == "--no-layer-shell");

    let mut cfg = LauncherConfig::load();
    if let Some(t) = theme_override.clone() {
        cfg.theme = t;
    }

    // Resolve + load theme with clear diagnostics; fall back so a window still appears.
    let theme_path = cfg.theme_path();
    let (theme, theme_error) = match Theme::load_from_dir(&theme_path) {
        Ok(t) => (t, None),
        Err(e) => (Theme::fallback(), Some(format!("{}: {e:#}", theme_path.display()))),
    };

    if args.iter().any(|a| a == "--validate-themes" || a == "--check-themes") {
        return validate_themes();
    }
    if args.iter().any(|a| a == "--screenshot" || a.starts_with("--screenshot=")) {
        return screenshot_mode(theme, &cfg, &args, size_override);
    }

    if let Some(err) = &theme_error {
        eprintln!("[hiren-client] WARNING: failed to load theme {err}");
        eprintln!("[hiren-client] WARNING: using built-in fallback theme");
    }

    let state = ObservableState::new(LauncherState::new());
    let bridge = hiren_client::ui_runtime::state_bridge::SearchBridge::new(&cfg);
    let core = AppCore::new(theme, state, bridge, cfg, size_override, reload);

    // Backend selection: real layer-shell on Wayland when available,
    // otherwise the portable winit toplevel (also used for X11).
    #[cfg(feature = "layer-shell")]
    {
        if !no_layer_shell && std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match hiren_client::wayland::run(core) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    eprintln!("[hiren-client] layer-shell backend failed ({e:#}); falling back to winit");
                    // Recreate what run() consumed: cheap enough at startup.
                    let cfg2 = LauncherConfig::load();
                    let mut cfg2 = cfg2;
                    if let Some(t) = theme_override.clone() {
                        cfg2.theme = t;
                    }
                    let theme2 = Theme::load_from_dir(&cfg2.theme_path()).unwrap_or_else(|_| Theme::fallback());
                    let state2 = ObservableState::new(LauncherState::new());
                    let bridge2 = hiren_client::ui_runtime::state_bridge::SearchBridge::new(&cfg2);
                    let core2 = AppCore::new(theme2, state2, bridge2, cfg2, size_override, reload);
                    return window::run(core2);
                }
            }
        }
    }
    let _ = no_layer_shell;
    window::run(core)
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == flag && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        if let Some(rest) = a.strip_prefix(&format!("{flag}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn print_help() {
    println!("hiren-client — declarative UI launcher");
    println!();
    println!("Usage: hiren-client [options]");
    println!("  --theme <name>        theme name (built-in or ~/.config/hiren/themes/<name>)");
    println!("  --width <px> --height <px>  override window size");
    println!("  --validate-themes     load + resolve + render all built-in themes, report issues");
    println!("  --list-themes         list available themes");
    println!("  --screenshot [theme]  render a theme offscreen to a PNG (no window)");
    println!("      --out <path>      output file (default /tmp/hiren-<theme>.png)");
    println!("      --query <text>    perform a real search instead of demo results");
    println!("  --no-layer-shell      force the winit toplevel backend");
    println!("  --reload              poll theme.toml for changes (dev convenience)");
    println!();
    println!("Themes are TOML scene graphs under hiren-client/themes/ or ~/.config/hiren/themes/.");
}

// ---------------------------------------------------------------------------
// Screenshot mode — headless validation: state → runtime → renderer → PNG
// ---------------------------------------------------------------------------

fn demo_state(state: &Rc<ObservableState>) {
    let entries = vec![
        hiren_shared::AppEntry::drun(
            "firefox".into(),
            "Firefox".into(),
            "firefox".into(),
            Some("Browse the Web".into()),
            "Internet WWW Browser".into(),
        ),
        hiren_shared::AppEntry::drun(
            "org.gnome.Files".into(),
            "Files".into(),
            "nautilus".into(),
            Some("Access and organize files".into()),
            "folder manager explore disk".into(),
        ),
        hiren_shared::AppEntry::run("foot".into(), "Foot terminal".into(), "foot".into()),
        hiren_shared::AppEntry::drun(
            "code".into(),
            "Visual Studio Code".into(),
            "code".into(),
            Some("Code Editing. Redefined.".into()),
            "editor ide development".into(),
        ),
        hiren_shared::AppEntry::run("calc".into(), "Calculator 4*7".into(), "gnome-calculator".into()),
    ];
    state.update(|s| {
        s.query = "f".into();
        s.set_results(entries);
        s.selected_index = 0;
    });
}

fn screenshot_mode(theme: Theme, cfg: &LauncherConfig, args: &[String], size_override: Option<(u32, u32)>) -> Result<()> {
    let theme_name = arg_value(args, "--screenshot").unwrap_or_else(|| cfg.theme.clone());
    let out = arg_value(args, "--out").unwrap_or_else(|| format!("/tmp/hiren-{theme_name}.png"));
    let query = arg_value(args, "--query");
    let settle_ms: u64 = arg_value(args, "--settle-ms").and_then(|v| v.parse().ok()).unwrap_or(900);

    // `--screenshot <name>` selects the theme (user themes take precedence).
    let theme = if theme_name == cfg.theme {
        theme
    } else {
        let mut cfg2 = cfg.clone();
        cfg2.theme = theme_name.clone();
        let path = cfg2.theme_path();
        match Theme::load_from_dir(&path) {
            Ok(t) => t,
            Err(e) => anyhow::bail!("cannot load theme `{theme_name}` from {}: {e:#}", path.display()),
        }
    };

    let state = ObservableState::new(LauncherState::new());
    match &query {
        Some(q) => {
            let bridge = hiren_client::ui_runtime::state_bridge::SearchBridge::new(cfg);
            bridge.search(q, &state);
        }
        None => demo_state(&state),
    }

    let size = size_override.unwrap_or_else(|| theme.window.effective_size((cfg.window_width.max(1) as u32, cfg.window_height.max(1) as u32)));
    let mut runtime = UiRuntime::new(theme, state.clone());

    // Warm resolve, let entrance animations settle, then capture.
    let _ = runtime.resolve(size);
    std::thread::sleep(std::time::Duration::from_millis(settle_ms));
    let nodes = runtime.resolve(size);
    for w in runtime.take_warnings() {
        eprintln!("[hiren-client] theme warning: {w}");
    }
    let count = nodes.nodes.len();
    if std::env::var_os("HIREN_DEBUG_NODES").is_some() {
        for n in &nodes.nodes {
            eprintln!("{:>3} {:<28} x={:>7.1} y={:>7.1} w={:>7.1} h={:>7.1} o={:.2} z={} rot={:.1} scale={:.2}",
                "", n.id, n.x, n.y, n.width, n.height, n.opacity, n.z, n.rotation, n.scale);
        }
    }
    let pixmap = runtime.render_nodes(&nodes.nodes, size, 1.0);
    let data = pixmap.encode_png().map_err(|e| anyhow::anyhow!("PNG encode: {e}"))?;
    std::fs::write(&out, data).map_err(|e| anyhow::anyhow!("write {out}: {e}"))?;
    println!("✓ {theme_name}: {count} nodes, {}x{} -> {out}", size.0, size.1);
    Ok(())
}

// ---------------------------------------------------------------------------
// Theme validation — load + resolve + render every built-in theme offscreen
// ---------------------------------------------------------------------------

fn validate_themes() -> Result<()> {
    let builtin_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("themes");
    let mut ok = true;
    let entries = std::fs::read_dir(&builtin_dir).map_err(|e| anyhow::anyhow!("read themes dir: {e}"))?;
    let mut names: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();

    for name in names {
        let path = builtin_dir.join(&name);
        match render_theme_check(&path) {
            Ok((desc, nodes, warnings)) => {
                println!("✓ {name}: {desc} — {nodes} draw nodes");
                for w in warnings {
                    println!("    ⚠ {w}");
                }
            }
            Err(e) => {
                eprintln!("✗ {name}: {e:#}");
                ok = false;
            }
        }
    }
    if ok {
        println!("All themes validated.");
        Ok(())
    } else {
        anyhow::bail!("Some themes failed validation")
    }
}

fn render_theme_check(path: &std::path::Path) -> Result<(String, usize, Vec<String>)> {
    let theme = Theme::load_from_dir(path)?;
    let desc = if theme.meta.description.is_empty() { theme.meta.name.clone() } else { theme.meta.description.clone() };

    let mut state = LauncherState::new();
    state.query = "test".into();
    state.set_results(vec![
        hiren_shared::AppEntry::drun("firefox".into(), "Firefox".into(), "firefox".into(), Some("Browse the Web".into()), "web".into()),
        hiren_shared::AppEntry::run("code".into(), "Code".into(), "code".into()),
        hiren_shared::AppEntry::run("alacritty".into(), "Alacritty".into(), "alacritty".into()),
        hiren_shared::AppEntry::run("calc".into(), "Calculator".into(), "calc".into()),
    ]);
    state.selected_index = 1;

    let size = theme.window.effective_size((640, 480));
    let mut runtime = UiRuntime::new(theme, ObservableState::new(state));
    let out = runtime.resolve(size);
    let nodes = out.nodes.clone();
    let warnings = runtime.take_warnings();
    let pixmap = runtime.render_nodes(&nodes, size, 1.0);
    let _ = pixmap; // exercising the full render path is the point
    Ok((desc, nodes.len(), warnings))
}
