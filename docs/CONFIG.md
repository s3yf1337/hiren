# hiren — конфигурация и использование

Современный лаунчер для Wayland с полностью декларативным UI: интерфейс —
TOML-сцена, рендер — tiny-skia, окна — нативный layer-shell (Wayland) или winit
(X11/запасной путь). Состоит из двух бинарников: `hiren-daemon` (фоновый
парсер `.desktop` + поиск по IPC) и `hiren-client` (сам лаунчер).

## Сборка

```bash
cargo build --release -p hiren-daemon
cargo build --release -p hiren-client --features layer-shell
```

GTK4 больше не требуется. Системных зависимостей нет: всё включено в бинарь,
кроме `libxkbcommon` (загружается динамически — есть в любой системе с Wayland).

## Зависимости (опционально)

| Утилита | Зачем |
|---|---|
| `wl-copy` / `xclip` | калькулятор копирует результат в буфер |
| `swaymsg` / `hyprctl` / `wmctrl` | режим переключения окон |
| терминал (`foot`, `kitty`, …) | `Ctrl+Enter` — запуск в терминале |

## Конфигурация

`~/.config/hiren/config.toml` — поведение (не внешность):

```toml
[ui]
width = 620                    # если в теме не задан свой размер
height = 360
auto_close_timeout_secs = 8    # 0 = не закрывать по таймауту
freq_weight = 0.8              # вклад частоты запусков в ранжирование
keyboard_mode = "exclusive"    # "exclusive" (захват клавиатуры) | "on_demand"
theme = "default"              # тема: встроенная или из ~/.config/hiren/themes/

[mode]
drun  = true   # приложения .desktop
run   = true   # исполняемые из $PATH
calc  = true   # калькулятор (результат копируется в буфер)
window = false # переключение окон

[terminal]
command = "foot"
exec_flag = "-e"

# Запуск через префикс: Ctrl+Exec запускает через команду ниже
[[bindings]]
key = "Return"

[[bindings]]
key = "Ctrl+Return"
prefix = 'env ALL_PROXY="socks5://127.0.0.1:2080"'
```

## Запуск

```bash
hiren-daemon &                         # обычно через systemd --user / exec-once
hiren-client                           # тема из конфига
hiren-client --theme atlus             # другая тема
hiren-client --no-layer-shell          # принудительно winit-окно
```

### Hyprland

```
exec-once = hiren-daemon
bind = $mainMod, space, exec, hiren-client
```

### Sway

```
exec_always hiren-daemon
bindsym $mod+space exec hiren-client
```

## Диагностика

```bash
hiren-client --validate-themes                 # проверить все темы без окна
hiren-client --list-themes                     # список доступных тем
hiren-client --screenshot my --out /tmp/p.png  # офлайн-рендер темы в PNG
RUST_LOG=hiren_client=debug hiren-client       # тайминги кадров
```

Проблемы:

- **Клиент не открывается** — смотрите stderr: ошибки темы печатаются, затем
  используется запасная тема. Проверьте `--validate-themes`.
- **Нет фокуса клавиатуры** — в конфиге `keyboard_mode = "exclusive"`;
  на не-wlroots композиторах используйте winit-режим (`--no-layer-shell`).
- **Тема не находится** — встроенные: `default, atlus, macos, layered,
  circular`; пользовательские лежат в `~/.config/hiren/themes/<имя>/theme.toml`.

## Темы

Внешний вид — это TOML-сцена: биндинги к состоянию лаунчера, компоненты,
репитеры с виртуализацией, анимации/springs, клик-действия. Полное описание
формата — в [CLIENT_ARCHITECTURE.md](CLIENT_ARCHITECTURE.md).

```bash
cp -r themes/default ~/.config/hiren/themes/my
$EDITOR ~/.config/hiren/themes/my/theme.toml
hiren-client --theme my    # без пересборки Rust
```
