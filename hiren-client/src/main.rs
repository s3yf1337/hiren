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
use hiren_client::window;
use std::rc::Rc;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse_args(&args);
    for warning in &cli.warnings {
        eprintln!("[hiren-client] WARNING: {warning}");
    }

    if cli.help {
        print_help();
        return Ok(());
    }
    if cli.list_themes {
        let themes = config::available_themes();
        for theme in &themes {
            println!("{theme}");
        }
        if themes.is_empty() {
            eprintln!("[hiren-client] WARNING: no themes found in the search paths");
        }
        return Ok(());
    }
    // Validate before loading the configured theme: validation itself reports
    // per-theme problems and must not be short-circuited by a broken selection.
    if cli.validate_themes {
        return validate_themes();
    }

    let mut cfg = LauncherConfig::load();
    if let Some(theme) = &cli.theme {
        cfg.theme = theme.clone();
    }

    if let Some(screenshot) = &cli.screenshot {
        return screenshot_mode(&cli, screenshot.as_deref(), &cfg);
    }

    let (theme, theme_warning) = load_theme(&cfg);
    if let Some(warning) = &theme_warning {
        eprintln!("[hiren-client] WARNING: {warning}");
    }

    let state = ObservableState::new(LauncherState::new());
    let bridge = hiren_client::ui_runtime::state_bridge::SearchBridge::new(&cfg);
    let core = AppCore::new(theme, state, bridge, cfg, cli.size_override(), cli.reload);

    // Backend selection: real layer-shell on Wayland when available,
    // otherwise the portable winit toplevel (also used for X11).
    #[cfg(feature = "layer-shell")]
    {
        if !cli.no_layer_shell && std::env::var_os("WAYLAND_DISPLAY").is_some() {
            match hiren_client::wayland::run(core) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    eprintln!("[hiren-client] layer-shell backend failed ({e:#}); falling back to winit");
                    // Recreate what run() consumed: cheap enough at startup.
                    let mut cfg2 = LauncherConfig::load();
                    if let Some(theme) = &cli.theme {
                        cfg2.theme = theme.clone();
                    }
                    let (theme2, _) = load_theme(&cfg2);
                    let state2 = ObservableState::new(LauncherState::new());
                    let bridge2 = hiren_client::ui_runtime::state_bridge::SearchBridge::new(&cfg2);
                    let core2 =
                        AppCore::new(theme2, state2, bridge2, cfg2, cli.size_override(), cli.reload);
                    return window::run(core2);
                }
            }
        }
    }
    let _ = cli.no_layer_shell;
    window::run(core)
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Разобранные аргументы командной строки.
#[derive(Debug, Clone, Default, PartialEq)]
struct Cli {
    help: bool,
    list_themes: bool,
    validate_themes: bool,
    theme: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    /// `None` — скриншот не запрошен; `Some(None)` — `--screenshot` без темы;
    /// `Some(Some(name))` — тема указана явно.
    screenshot: Option<Option<String>>,
    out: Option<String>,
    query: Option<String>,
    settle_ms: Option<u64>,
    no_layer_shell: bool,
    reload: bool,
    /// Некритичные проблемы парсинга (неизвестные флаги, битые значения).
    warnings: Vec<String>,
}

impl Cli {
    /// `--width/--height` работают только парой: одиночное значение не даёт
    /// согласованного размера и раньше молча игнорировалось.
    fn size_override(&self) -> Option<(u32, u32)> {
        match (self.width, self.height) {
            (Some(w), Some(h)) => Some((w, h)),
            _ => None,
        }
    }
}

fn parse_dimension(flag: &str, value: &str, warnings: &mut Vec<String>) -> Option<u32> {
    match value.parse::<i64>() {
        Ok(v) if (1..=config::MAX_WINDOW_DIMENSION as i64).contains(&v) => Some(v as u32),
        Ok(v) => {
            let clamped = v.clamp(1, config::MAX_WINDOW_DIMENSION as i64) as u32;
            warnings.push(format!(
                "{flag} {v} is out of range 1..={}; using {clamped}",
                config::MAX_WINDOW_DIMENSION
            ));
            Some(clamped)
        }
        Err(_) => {
            warnings.push(format!("{flag} expects a number, got `{value}`; ignoring it"));
            None
        }
    }
}

/// Разобрать argv. Функция не завершает процесс и не читает глобальное
/// состояние — все проблемы попадают в `warnings`.
fn parse_args(args: &[String]) -> Cli {
    let mut cli = Cli::default();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => cli.help = true,
            "--list-themes" => cli.list_themes = true,
            "--validate-themes" | "--check-themes" => cli.validate_themes = true,
            "--no-layer-shell" => cli.no_layer_shell = true,
            "--reload" => cli.reload = true,
            "--theme" => match iter.next() {
                Some(value) => cli.theme = Some(value.clone()),
                None => cli.warnings.push("--theme requires a value".into()),
            },
            "--width" => match iter.next() {
                Some(value) => cli.width = parse_dimension("--width", value, &mut cli.warnings),
                None => cli.warnings.push("--width requires a value".into()),
            },
            "--height" => match iter.next() {
                Some(value) => cli.height = parse_dimension("--height", value, &mut cli.warnings),
                None => cli.warnings.push("--height requires a value".into()),
            },
            "--out" => match iter.next() {
                Some(value) => cli.out = Some(value.clone()),
                None => cli.warnings.push("--out requires a value".into()),
            },
            "--query" => match iter.next() {
                Some(value) => cli.query = Some(value.clone()),
                None => cli.warnings.push("--query requires a value".into()),
            },
            "--settle-ms" => match iter.next() {
                Some(value) => match value.parse::<u64>() {
                    Ok(ms) => cli.settle_ms = Some(ms),
                    Err(_) => cli.warnings.push(format!(
                        "--settle-ms expects a number, got `{value}`; ignoring it"
                    )),
                },
                None => cli.warnings.push("--settle-ms requires a value".into()),
            },
            "--screenshot" => {
                // Опциональная тема: следующий токен — тема, только если это
                // не другой флаг (`--screenshot --out x` НЕ должно съедать --out).
                let theme = iter
                    .peek()
                    .filter(|next| !next.starts_with('-'))
                    .map(|next| next.to_string());
                if theme.is_some() {
                    iter.next();
                }
                cli.screenshot = Some(theme);
            }
            other if other.starts_with("--theme=") => {
                cli.theme = Some(other.trim_start_matches("--theme=").to_string());
            }
            other if other.starts_with("--out=") => {
                cli.out = Some(other.trim_start_matches("--out=").to_string());
            }
            other if other.starts_with("--query=") => {
                cli.query = Some(other.trim_start_matches("--query=").to_string());
            }
            other if other.starts_with("--width=") => {
                cli.width =
                    parse_dimension("--width", other.trim_start_matches("--width="), &mut cli.warnings);
            }
            other if other.starts_with("--height=") => {
                cli.height = parse_dimension(
                    "--height",
                    other.trim_start_matches("--height="),
                    &mut cli.warnings,
                );
            }
            other if other.starts_with("--screenshot=") => {
                let theme = other.trim_start_matches("--screenshot=");
                cli.screenshot = Some(if theme.is_empty() { None } else { Some(theme.to_string()) });
            }
            other if other.starts_with('-') => {
                cli.warnings.push(format!("unknown flag `{other}` (ignored)"));
            }
            other => {
                cli.warnings.push(format!("unexpected argument `{other}` (ignored)"));
            }
        }
    }

    if cli.width.is_some() != cli.height.is_some() {
        cli.warnings.push(
            "--width and --height must be given together; ignoring the partial size override"
                .into(),
        );
        cli.width = None;
        cli.height = None;
    }
    cli
}

fn print_help() {
    println!("hiren-client — declarative UI launcher");
    println!();
    println!("Usage: hiren-client [options]");
    println!("  --theme <name>        theme name (searched: ~/.config/hiren/themes/<name>,");
    println!("                        <exe-dir>/themes, <prefix>/share/hiren/themes,");
    println!("                        $XDG_DATA_DIRS/hiren/themes)");
    println!("  --width <px> --height <px>  override window size (pair, 1..={})", config::MAX_WINDOW_DIMENSION);
    println!("  --validate-themes     load + resolve + render all discovered themes, report issues");
    println!("  --list-themes         list available themes");
    println!("  --screenshot [theme]  render a theme offscreen to a PNG (no window);");
    println!("                        the theme argument is optional and `--screenshot=name` works too");
    println!("      --out <path>      output file (default /tmp/hiren-<theme>.png)");
    println!("      --query <text>    perform a real search instead of demo results");
    println!("      --settle-ms <n>   wait n ms for entrance animations (default 900)");
    println!("  --no-layer-shell      force the winit toplevel backend");
    println!("  --reload              poll theme.toml for changes (dev convenience)");
    println!("  -h, --help            show this help");
    println!();
    println!("Themes are TOML scene graphs under hiren-client/themes/, installed share dirs");
    println!("or ~/.config/hiren/themes/.");
}

// ---------------------------------------------------------------------------
// Theme loading
// ---------------------------------------------------------------------------

/// Загрузить выбранную тему; при отсутствии/ошибке — громкое предупреждение и
/// встроенный fallback (окно всё равно появится). Возвращает текст
/// предупреждения для печати.
fn load_theme(config: &LauncherConfig) -> (Theme, Option<String>) {
    let name = &config.theme;
    match config::find_theme(name) {
        Some(path) => match Theme::load_from_dir(&path) {
            Ok(theme) => (theme, None),
            Err(e) => (
                Theme::fallback(),
                Some(format!(
                    "failed to load theme `{name}` from {}: {e:#} — using built-in fallback",
                    path.display()
                )),
            ),
        },
        None => {
            let searched: Vec<String> = config::theme_roots()
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            (
                Theme::fallback(),
                Some(format!(
                    "theme `{name}` not found (searched: {}); using built-in fallback",
                    searched.join(", ")
                )),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Screenshot mode — headless validation: state → runtime → renderer → PNG
// ---------------------------------------------------------------------------

fn demo_state(state: &Rc<ObservableState>) {
    let mut entries = vec![
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
    for i in 0..10 {
        entries.push(hiren_shared::AppEntry::drun(
            format!("app{i}"),
            format!("Filler App {i}"),
            format!("app{i}"),
            Some("QA filler entry for scroll testing".into()),
            String::new(),
        ));
    }
    state.update(|s| {
        s.query = "f".into();
        s.set_results(entries);
        s.selected_index = 0;
    });
}

fn screenshot_mode(cli: &Cli, cli_theme: Option<&str>, cfg: &LauncherConfig) -> Result<()> {
    let theme_name = cli_theme.map(str::to_string).unwrap_or_else(|| cfg.theme.clone());

    // Отсутствующая тема — громкая ошибка, а не молчаливый рендер fallback'а
    // с печатью успеха (репро: `--screenshot <missing>` печатал ✓).
    let Some(theme_path) = config::find_theme(&theme_name) else {
        let available = config::available_themes();
        anyhow::bail!(
            "theme `{theme_name}` not found\n  searched: {}\n  available: {}",
            config::theme_roots()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            if available.is_empty() { "<none>".to_string() } else { available.join(", ") }
        );
    };
    let theme = Theme::load_from_dir(&theme_path)
        .map_err(|e| anyhow::anyhow!("cannot load theme `{theme_name}` from {}: {e:#}", theme_path.display()))?;

    let out = cli
        .out
        .clone()
        .unwrap_or_else(|| format!("/tmp/hiren-{theme_name}.png"));
    let settle_ms = cli.settle_ms.unwrap_or(900);

    let state = ObservableState::new(LauncherState::new());
    match &cli.query {
        Some(query) => {
            let bridge = hiren_client::ui_runtime::state_bridge::SearchBridge::new(cfg);
            bridge.search(query, &state);
        }
        None => demo_state(&state),
    }

    let size = cli.size_override().unwrap_or_else(|| {
        theme
            .window
            .effective_size((cfg.window_width.max(1) as u32, cfg.window_height.max(1) as u32))
    });
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
// Theme validation — load + resolve + render every discovered theme offscreen
// ---------------------------------------------------------------------------

fn validate_themes() -> Result<()> {
    let themes = config::available_theme_paths();
    if themes.is_empty() {
        // Отсутствие встроенного каталога не ошибка: темы могут быть только
        // пользовательскими/установленными, либо их нет вовсе.
        eprintln!(
            "✗ no themes found (searched: {})",
            config::theme_roots()
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }

    let total = themes.len();
    let mut ok = true;
    for (name, path) in themes {
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
        println!("All {total} themes validated.");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn screenshot_does_not_swallow_next_flag() {
        // Репро: `--screenshot --out /tmp/x.png` брал "--out" как имя темы.
        let cli = parse_args(&args(&["--screenshot", "--out", "/tmp/x.png"]));
        assert_eq!(cli.screenshot, Some(None));
        assert_eq!(cli.out.as_deref(), Some("/tmp/x.png"));
        assert!(cli.warnings.is_empty());
    }

    #[test]
    fn screenshot_accepts_optional_theme() {
        let cli = parse_args(&args(&["--screenshot", "atlus", "--out", "a.png"]));
        assert_eq!(cli.screenshot, Some(Some("atlus".into())));
        assert_eq!(cli.out.as_deref(), Some("a.png"));

        let cli = parse_args(&args(&["--screenshot=macos"]));
        assert_eq!(cli.screenshot, Some(Some("macos".into())));

        let cli = parse_args(&args(&["--screenshot="]));
        assert_eq!(cli.screenshot, Some(None));
    }

    #[test]
    fn theme_flag_forms() {
        assert_eq!(parse_args(&args(&["--theme", "atlus"])).theme.as_deref(), Some("atlus"));
        assert_eq!(parse_args(&args(&["--theme=atlus"])).theme.as_deref(), Some("atlus"));
        let cli = parse_args(&args(&["--theme"]));
        assert_eq!(cli.theme, None);
        assert!(!cli.warnings.is_empty());
    }

    #[test]
    fn width_height_require_pair() {
        let cli = parse_args(&args(&["--width", "800", "--height", "500"]));
        assert_eq!(cli.size_override(), Some((800, 500)));

        let cli = parse_args(&args(&["--width", "800"]));
        assert_eq!(cli.size_override(), None);
        assert!(cli.warnings.iter().any(|w| w.contains("together")));
    }

    #[test]
    fn huge_dimensions_are_clamped_not_accepted() {
        // Репро: --width 4000000000 → panic в renderer.
        let cli = parse_args(&args(&["--width", "4000000000", "--height", "360"]));
        assert_eq!(
            cli.size_override(),
            Some((config::MAX_WINDOW_DIMENSION as u32, 360))
        );
        assert!(cli.warnings.iter().any(|w| w.contains("out of range")));

        let cli = parse_args(&args(&["--width", "wide", "--height", "360"]));
        assert_eq!(cli.size_override(), None);
        assert!(cli.warnings.iter().any(|w| w.contains("expects a number")));
    }

    #[test]
    fn zero_dimension_is_clamped_to_one() {
        let cli = parse_args(&args(&["--width", "0", "--height", "-5"]));
        assert_eq!(cli.size_override(), Some((1, 1)));
    }

    #[test]
    fn unknown_flags_and_positionals_are_warned_not_fatal() {
        let cli = parse_args(&args(&["--wat", "positional", "--validate-themes"]));
        assert!(cli.validate_themes);
        assert!(cli.warnings.iter().any(|w| w.contains("unknown flag")));
        assert!(cli.warnings.iter().any(|w| w.contains("unexpected argument")));
    }

    #[test]
    fn mode_flags_parse() {
        let cli = parse_args(&args(&[
            "--validate-themes",
            "--no-layer-shell",
            "--reload",
            "--query",
            "fire",
            "--settle-ms",
            "1200",
        ]));
        assert!(cli.validate_themes);
        assert!(cli.no_layer_shell);
        assert!(cli.reload);
        assert_eq!(cli.query.as_deref(), Some("fire"));
        assert_eq!(cli.settle_ms, Some(1200));
    }

    #[test]
    fn bad_settle_ms_is_warned() {
        let cli = parse_args(&args(&["--screenshot", "--settle-ms", "soon"]));
        assert_eq!(cli.settle_ms, None);
        assert!(cli.warnings.iter().any(|w| w.contains("--settle-ms")));
    }

    #[test]
    fn help_flag() {
        assert!(parse_args(&args(&["--help"])).help);
        assert!(parse_args(&args(&["-h"])).help);
    }

    #[test]
    fn validate_flag_aliases() {
        assert!(parse_args(&args(&["--check-themes"])).validate_themes);
        assert!(parse_args(&args(&["--validate-themes"])).validate_themes);
    }
}
