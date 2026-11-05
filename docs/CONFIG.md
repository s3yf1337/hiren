# hiren — конфигурация и использование

Современный лаунчер для Wayland с полностью декларативным UI: интерфейс —
TOML-сцена, рендер — tiny-skia, окна — нативный layer-shell (Wayland) или winit
(X11/запасной путь). Состоит из двух бинарников: `hiren-daemon` (фоновый
парсер `.desktop` + поиск по IPC) и `hiren-client` (сам лаунчер).

## Сборка и установка

```bash
cargo build --release -p hiren-daemon
cargo build --release -p hiren-client --features layer-shell

sudo make install                 # PREFIX=/usr/local по умолчанию
# или
sudo make install PREFIX=/usr

systemctl --user daemon-reload
systemctl --user enable --now hiren-daemon
```

`make install` кладёт бинарники в `$(PREFIX)/bin`, полные каталоги тем — в
`$(PREFIX)/share/hiren/themes/`, unit — в
`~/.config/systemd/user/hiren-daemon.service` (под `sudo` — в домашний каталог
пользователя, запустившего `sudo`). `DESTDIR` влияет только на пути файлов и
никогда не попадает в содержимое unit-файла.

GTK4 не требуется. Системных зависимостей нет: всё включено в бинарь, кроме
`libxkbcommon` (загружается динамически — есть в любой системе с Wayland).

## Зависимости (опционально)

| Утилита | Зачем |
|---|---|
| `wl-copy` / `xclip` | калькулятор копирует результат в буфер |
| `swaymsg` / `hyprctl` / `wmctrl` | режим переключения окон |
| терминал (`foot`, `kitty`, …) | `Ctrl+Return` и приложения с `Terminal=true` |

## Конфигурация

`$XDG_CONFIG_HOME/hiren/config.toml` (обычно `~/.config/hiren/config.toml`) —
поведение, не внешность:

```toml
[ui]
width = 620                    # запасной размер, если тема не задала свой
height = 360                   # оба значения зажимаются в 1..=8192
auto_close_timeout_secs = 8    # 0 = не закрывать по таймауту
freq_weight = 0.8              # вклад частоты запусков, зажимается в 0..=10
keyboard_mode = "exclusive"    # "exclusive" (захват клавиатуры) | "on_demand"

[mode]
drun  = true   # приложения .desktop (по умолчанию true)
run   = true   # исполняемые из $PATH (по умолчанию false)
calc  = true   # калькулятор, результат копируется в буфер (по умолчанию false)
window = false # переключение окон (по умолчанию false)

[window]
list_command = "swaymsg -t get_tree"
activate_command = 'swaymsg "[con_id={id}] focus"'

[terminal]
command = "foot"     # определяется автоматически, если секции нет
exec_flag = "-e"

[[bindings]]
key = "Return"

[[bindings]]
key = "Ctrl+Return"
prefix = 'env ALL_PROXY="socks5://127.0.0.1:2080"'

[theme]
name = "default"     # встроенная тема или тема из ~/.config/hiren/themes/
```

### Секции и значения

- `[ui] width/height` — запасной размер окна: тема может задать свой. Значения
  зажимаются в `1..=8192`; огромные числа не приводят к панике.
- `[ui] auto_close_timeout_secs` — авто-закрытие; `0` отключает таймаут.
- `[ui] freq_weight` — вес частоты запусков в ранжировании, зажимается в
  `0..=10`.
- `[ui] keyboard_mode` — `exclusive` (захват клавиатуры layer-shell) или
  `on_demand`. Неизвестное значение даёт предупреждение и `exclusive`.
- `[mode]` — какие источники результатов включены. Если секции нет, включён
  только `drun`; внутри секции отсутствующие ключи берут значения по умолчанию.
- `[terminal]` — команда и флаг для запуска в терминале. Используется
  приложениями с `Terminal=true` и стандартной привязкой `Ctrl+Return`.
  Если секции нет, клиент ищет `$TERMINAL`, затем `foot`, `alacritty`, `kitty`,
  `wezterm`, `gnome-terminal`, `konsole`, `xfce4-terminal`, `xterm`.
- `[[bindings]]` — список клавиш. Если список пуст или отсутствует, действуют
  две стандартные привязки: `Return` и `Ctrl+Return`. Явно заданный список
  заменяет стандартный. `prefix` — произвольная команда-префикс; для
  `Ctrl+Return` без `prefix` префикс собирается из `[terminal] command` +
  `exec_flag` (например, `foot -e`). Явный `prefix` всегда важнее.
- `[window]` — команды получения списка окон и активации для режима `window`.
- `[theme] name` — имя темы. **Не** `[ui] theme`: старый вариант был ошибкой
  документации; теперь это неизвестный ключ, о котором клиент предупреждает.

Конфиг загружается терпимо: каждая секция разбирается отдельно. Сломанная
секция откатывается к значениям по умолчанию с предупреждением в stderr,
остальные секции продолжают работать. Неизвестные ключи и неизвестные
модификаторы клавиш тоже дают предупреждения, а не молча игнорируются.

## Запуск

```bash
hiren-daemon &                         # обычно через systemd --user / exec-once
hiren-client                           # тема из конфига
hiren-client --theme atlus             # другая тема
hiren-client --no-layer-shell          # принудительно winit-окно
```

### Командная строка

| Опция | Поведение |
|---|---|
| `--theme <name>` | тема на этот запуск (встроенная или пользовательская) |
| `--width <px> --height <px>` | переопределение размера окна; нужны оба значения, иначе опция игнорируется; значения зажимаются в `1..=8192` |
| `--list-themes` | напечатать все найденные темы |
| `--validate-themes` | загрузить, разрешить и отрендерить все встроенные **и пользовательские** темы; предупреждения и ошибки по каждой теме, ненулевой код возврата при провале |
| `--screenshot [THEME] --out FILE [--query Q] [--settle-ms N]` | офлайн-рендер в PNG. `THEME` необязателен (по умолчанию тема из конфига). Если следующий аргумент начинается с `--`, он не съедается как имя темы: `--screenshot --out x.png` корректно. `--out` по умолчанию `/tmp/hiren-<theme>.png`, `--settle-ms` — 900 |
| `--no-layer-shell` | принудительный winit-бэкенд |
| `--reload` | опрос `theme.toml` на изменения (разработка) |
| `--help` | краткая справка |

`--query` выполняет настоящий поиск (для результатов из `.desktop` нужен
запущенный демон). `HIREN_DEBUG_NODES=1` вместе с `--screenshot` печатает
геометрию разрешённых узлов. `RUST_LOG=hiren_client=debug` включает тайминги
кадров.

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

## Сокет и IPC

Демон слушает `$XDG_RUNTIME_DIR/hiren.socket`:

- каталог `$XDG_RUNTIME_DIR` приватный (0700), сокет создаётся с правами `0600`;
- демон проверяет uid подключившегося процесса (peer credentials) и отвергает
  чужие подключения;
- одновременно работает один экземпляр на пользователя; устаревший сокет
  удаляется при старте, живой — приводит к понятной ошибке второго экземпляра;
- `HIREN_SOCKET_PATH` переопределяет путь для тестов и нестандартных схем;
  значение должно совпадать у демона и клиента.

Протокол — 4-байтовая little-endian длина кадра + JSON. Ограничения:

- максимальный кадр — **256 КиБ**; больший кадр отвергается и соединение
  закрывается;
- длина поискового запроса ограничена, лишний ввод не отправляется демону;
- число одновременных подключений и время чтения/записи ограничены; медленное
  или переразмерное соединение обрывается;
- ошибки протокола/демона не теряются: клиент печатает их в stderr и выставляет
  `launcher.error`, который тема может отобразить.

## Темы

Внешний вид — это TOML-сцена: биндинги к состоянию лаунчера, компоненты,
репитеры с виртуализацией, анимации/springs, клик-действия. Полное описание
формата — в [CLIENT_ARCHITECTURE.md](CLIENT_ARCHITECTURE.md).

Встроенные темы: `default`, `atlus`, `circular`, `layered`, `macos`.

Порядок поиска темы:

1. `~/.config/hiren/themes/<name>/`;
2. `themes/` рядом с бинарником клиента;
3. `$(PREFIX)/share/hiren/themes/<name>/`;
4. `$XDG_DATA_DIRS/hiren/themes/<name>/` (`/usr/local/share`, Flatpak и т.п.).

Пользовательская тема важнее встроенной. Если запрошенная тема не найдена,
клиент громко предупреждает (имя, пути поиска, доступные темы) и показывает
запасной вариант — молчаливого переключения на `default` больше нет.

Схема темы строгая: неизвестные поля и неверные типы дают предупреждение или
ошибку загрузки, размеры окна и параметры spring проверяются. Встроенные темы
обязаны проходить `--validate-themes`; сломанную тему удобно ловить этой
командой.

```bash
cp -r hiren-client/themes/default ~/.config/hiren/themes/my
$EDITOR ~/.config/hiren/themes/my/theme.toml
hiren-client --theme my    # без пересборки Rust
```

Копируйте каталог темы целиком: `fonts/`, `assets/` и другие данные нужны
рантайму.

## Поведение демона и `.desktop`

- Каталоги `applications/` из `XDG_DATA_HOME` (по умолчанию
  `~/.local/share`) и всех `XDG_DATA_DIRS` (по умолчанию
  `/usr/local/share:/usr/share`) сканируются рекурсивно. Это покрывает
  Flatpak-экспорты и локальные установки в `/usr/local/share`.
- Приоритет — по desktop-id: запись из более раннего каталога побеждает.
  `Hidden=true` в более приоритетном каталоге работает как tombstone и скрывает
  одноимённую запись из каталогов ниже.
- Раскрываются field-коды `Exec`: `%c` — локализованное имя, `%i` — пара
  `--icon <icon>`, `%k` — путь к `.desktop`; `%f/%F/%u/%U` и остальные
  файловые коды отбрасываются, `%%` превращается в `%`.
- `Name[...]` выбирается по локали (`LANG`/`LC_MESSAGES`) с откатом на
  непереведённый `Name`.
- `OnlyShowIn`/`NotShowIn` учитывают `XDG_CURRENT_DESKTOP`.
- `NoDisplay=true` и не прошедший `TryExec` скрываются; `Terminal=true`
  означает запуск через `[terminal] command` + `exec_flag`.

## Диагностика

```bash
hiren-client --validate-themes                    # все темы, включая пользовательские
hiren-client --list-themes                        # список доступных тем
hiren-client --screenshot my --out /tmp/p.png     # офлайн-рендер темы в PNG
RUST_LOG=hiren_client=debug hiren-client          # тайминги кадров
```

Проблемы:

- **Тема не находится** — проверьте `--list-themes` и пути из раздела «Темы»;
  клиент печатает предупреждение с именем запрошенной темы.
- **Клиент не открывается** — смотрите stderr: ошибки темы печатаются, затем
  используется запасная тема. Проверьте `--validate-themes`.
- **Демон не отвечает** — проверьте `systemctl --user status hiren-daemon`,
  путь сокета (`$XDG_RUNTIME_DIR/hiren.socket`) и `HIREN_SOCKET_PATH`; ошибка
  также видна в UI через `launcher.error`.
- **Нет фокуса клавиатуры** — в конфиге `keyboard_mode = "exclusive"`;
  на не-wlroots композиторах используйте winit-режим (`--no-layer-shell`).
