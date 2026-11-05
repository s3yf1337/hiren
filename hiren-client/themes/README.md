# Themes

Each directory is a self-contained UI theme — a TOML scene graph rendered by the
same `hiren-client` runtime. A theme directory may contain `theme.toml`,
`fonts/`, `assets/`, and any other files the scene references; keep the whole
directory together when copying or installing.

Bundled themes (five):

- `default` — clean vertical launcher (practical default, proves the runtime replaces the old GTK UI)
- `atlus` — Metaphor-inspired archive menu: cream manuscript + navy void, independent crimson selector, compositional selection, OFL display fonts in `atlus/fonts/`
- `circular` — radial: search centered, results on a circle via `layout="circular"` + `cos/sin` bindings
- `layered` — two panels + depth orbs reacting to selection
- `macos` — calm floating card, blur hint, spring selector

Validate without opening a window (bundled **and** user themes are rendered
headlessly; failures exit non-zero):

```bash
cargo run -p hiren-client -- --validate-themes
cargo run -p hiren-client -- --list-themes
cargo run -p hiren-client -- --theme circular
```

Theme discovery order: `~/.config/hiren/themes/<name>/`, then `themes/` next
to the client binary, then `$(PREFIX)/share/hiren/themes/<name>/`, then
`$XDG_DATA_DIRS/hiren/themes/<name>/`. A missing requested theme warns loudly
instead of silently falling back.

The schema is strict: unknown fields and wrong types are reported, and window
sizes and spring parameters are validated. Run `--validate-themes` after
editing.

Create your own:

```bash
cp -r hiren-client/themes/default ~/.config/hiren/themes/my
# edit ~/.config/hiren/themes/my/theme.toml (and keep fonts/assets alongside)
# in ~/.config/hiren/config.toml set [theme] name = "my"
hiren-client --theme my
```

See `docs/CLIENT_ARCHITECTURE.md` for full binding, component, animation, and Wayland docs.
