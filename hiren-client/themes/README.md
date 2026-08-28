# Themes

Each directory is a self-contained UI theme — a TOML scene graph rendered by the same `hiren-client` runtime.

- `default` — clean vertical launcher (practical default, proves runtime replaces old GTK UI)
- `atlus` — game-like, bold, skewed, independent selector with spring, preview panel
- `macos` — calm floating card, blur hint, spring selector
- `layered` — two panels + depth orbs reacting to selection
- `circular` — radial, search centered, results in circle via `layout="circular"` + `cos/sin` bindings

Validate without opening a window:

```bash
cargo run -p hiren-client -- --validate-themes
cargo run -p hiren-client -- --list-themes
cargo run -p hiren-client -- --theme circular
```

Create your own:

```bash
cp -r hiren-client/themes/default ~/.config/hiren/themes/my
# edit ~/.config/hiren/themes/my/theme.toml
# in ~/.config/hiren/config.toml set [theme] name = "my"
hiren-client --theme my
```

See `docs/CLIENT_ARCHITECTURE.md` for full binding, component, animation, and Wayland docs.
