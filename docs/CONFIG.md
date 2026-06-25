# hiren — Wayland application launcher

Современный, быстрый и кастомизируемый лаунчер приложений для Wayland.
Состоит из двух независимых бинарников: фонового демона (`hiren-daemon`)
и графического клиента (`hiren-client`), общающихся через UNIX-сокет.

## Оглавление

1. [Быстрый старт](#быстрый-старт)
2. [Конфигурация](#конфигурация)
3. [CSS-стилизация](#css-стилизация)
4. [Горячие клавиши](#горячие-клавиши)
5. [Архитектура и IPC](#архитектура-и-ipc)
6. [Парсинг .desktop-файлов](#парсинг-desktop-файлов)
7. [Решение проблем](#решение-проблем)

---

## Быстрый старт

### Зависимости системы

```bash
# Arch Linux
sudo pacman -S gtk4 gtk4-layer-shell

# Ubuntu/Debian
sudo apt install libgtk-4-dev libgtk4-layer-shell-dev

# Fedora
sudo dnf install gtk4-devel gtk4-layer-shell-devel
```

### Сборка

```bash
cd hiren
cargo build --release
```

### Запуск

```bash
# Терминал 1 — демон (должен работать всегда в фоне)
RUST_LOG=info ./target/release/hiren-daemon

# Терминал 2 — клиент (или привяжите к горячей клавише в композиторе)
./target/release/hiren-client
```

Демон слушает UNIX-сокет `/tmp/hiren.socket`. Клиент подключается к нему
автоматически. Если демон не запущен, клиент выведет ошибку подключения.

---

## Конфигурация

Файл конфигурации: **`~/.config/hiren/config.toml`**

При первом запуске клиента файл НЕ создаётся автоматически — используется
конфигурация по умолчанию. Чтобы переопределить настройки, создайте файл
вручную.

### Формат

```toml
# Бинды запуска. key = модификаторы+клавиша, prefix = что подставить перед Exec.
# Если prefix пустой или отсутствует — прямой запуск без префикса.
# Бинды проверяются по порядку — первый совпавший используется.

[[bindings]]
key = "Return"
# prefix не указан → обычный запуск

[[bindings]]
key = "Ctrl+Return"
prefix = "proxychains"

[[bindings]]
key = "Shift+Return"
prefix = "foot --"

[[bindings]]
key = "Alt+Return"
prefix = "systemd-run --user --scope"

[ui]
# Авто-закрытие через N секунд бездействия (0 = отключено)
auto_close_timeout_secs = 8
# Размеры окна в пикселях
width = 620
height = 360
```

Если секция `[[bindings]]` отсутствует — используются дефолтные бинды:
- `Enter` — обычный запуск (без префикса)
- `Ctrl+Enter` — обычный запуск (без префикса)

> **Важно о кавычках в TOML**: если `prefix` содержит двойные кавычки (`"`),
> спецсимволы или `\`, используйте **одинарные кавычки**: `prefix = 'команда с "кавычками"'`.
> Двойные кавычки `"…"` требуют экранирования: `prefix = "команда с \"кавычками\""`.

### Все параметры

| Секция | Ключ | Тип | По умолчанию | Описание |
|--------|------|-----|-------------|----------|
| `[[bindings]]` | `key` | `String` | — | Комбинация клавиш: `"Return"`, `"Ctrl+Return"`, `"Shift+A"`, `"Super+F1"` и т.д. |
| `[[bindings]]` | `prefix` | `String` (или отсутствует) | `None` | Префикс, подставляемый перед `Exec`: `"proxychains"`, `"foot --"`, `"systemd-run --user --scope"` |
| `[ui]` | `auto_close_timeout_secs` | `Integer` (или отсутствует) | `8` | Авто-закрытие через N секунд бездействия. `0` отключает таймер |
| `[ui]` | `width` | `Integer` (или отсутствует) | `620` | Ширина окна лаунчера в пикселях |
| `[ui]` | `height` | `Integer` (или отсутствует) | `360` | Высота окна лаунчера в пикселях |

### Поддерживаемые модификаторы

| Модификатор | Алиасы | Описание |
|-------------|--------|----------|
| `Ctrl` | `Control` | Control (Ctrl) |
| `Shift` | — | Shift |
| `Alt` | — | Alt (Mod1) |
| `Super` | `Win`, `Mod4` | Super/Windows key |
| `Meta` | — | Meta key |

### Поддерживаемые имена клавиш

Все имена **case-insensitive**.

| Имя клавиши | Алиасы | Keyval | Описание |
|-------------|--------|--------|----------|
| `Return` | `Enter` | `0xFF0D` | Enter / Return |
| `KP_Enter` | `KPEnter` | `0xFF8D` | NumPad Enter |
| `Escape` | `Esc` | `0xFF1B` | Escape |
| `Tab` | — | `0xFF09` | Tab |
| `Space` | — | `0x0020` | Пробел |
| `BackSpace` | — | `0xFF08` | Backspace |
| `Delete` | `Del` | `0xFFFF` | Delete |
| `Up` | — | `0xFF52` | Стрелка вверх |
| `Down` | — | `0xFF54` | Стрелка вниз |
| `Left` | — | `0xFF51` | Стрелка влево |
| `Right` | — | `0xFF53` | Стрелка вправо |
| `Home` | — | `0xFF50` | Home |
| `End` | — | `0xFF57` | End |
| `Page_Up` | `PageUp` | `0xFF55` | Page Up |
| `Page_Down` | `PageDown` | `0xFF56` | Page Down |
| `F1`–`F12` | — | `0xFFBE`–`0xFFC9` | Функциональные клавиши |
| `minus` | — | `0x002D` | `-` |
| `equal` | — | `0x003D` | `=` |
| `bracketleft` | — | `0x005B` | `[` |
| `bracketright` | — | `0x005D` | `]` |
| `backslash` | — | `0x005C` | `\` |
| `semicolon` | — | `0x003B` | `;` |
| `apostrophe` | `quote` | `0x0027` | `'` |
| `comma` | — | `0x002C` | `,` |
| `period` | — | `0x002E` | `.` |
| `slash` | — | `0x002F` | `/` |
| `grave` | — | `0x0060` | `` ` `` |
| `a`–`z`, `A`–`Z` | — | `0x0041`–`0x005A` | Буквы (keyval всегда uppercase) |
| `0`–`9` | — | `0x0030`–`0x0039` | Цифры |

### Примеры конфигов

**Запуск с proxychains по Ctrl+Enter:**
```toml
[[bindings]]
key = "Return"

[[bindings]]
key = "Ctrl+Return"
prefix = "proxychains"
```

**Запуск в терминале foot по Shift+Enter:**
```toml
[[bindings]]
key = "Return"

[[bindings]]
key = "Shift+Return"
prefix = "foot --"
```

**Запуск в systemd-scope по Alt+Enter:**
```toml
[[bindings]]
key = "Return"

[[bindings]]
key = "Alt+Return"
prefix = "systemd-run --user --scope"
```

**Полный пример со всеми секциями:**
```toml
[[bindings]]
key = "Return"

[[bindings]]
key = "Ctrl+Return"
prefix = "proxychains"

[[bindings]]
key = "Super+Return"
prefix = "foot --"

[ui]
auto_close_timeout_secs = 15
width = 800
height = 400
```

**Пустой файл (всё по умолчанию):**
```toml
# Все настройки используются со значениями по умолчанию
# Enter и Ctrl+Enter — обычный запуск без префикса
```

**Префикс с переменными окружения и кавычками:**
```toml
[[bindings]]
key = "Ctrl+Return"
# Используйте ОДИНАРНЫЕ кавычки для строк, содержащих двойные кавычки
# или спецсимволы — TOML не разрешает неэкранированные " внутри "…".
prefix = 'env ALL_PROXY="socks5h://127.0.0.1:2080"'
```

**Совместимость со старым форматом:**
```toml
# Старая секция [shortcuts] всё ещё парсится (без ошибок),
# но игнорируется — используйте [[bindings]].
[shortcuts]
ctrl_enter_prefix = "proxychains"  # ← больше не работает, используйте [[bindings]]
```

---

## CSS-стилизация

Файл стилей: **`~/.config/hiren/style.css`**

Если файл отсутствует, используется встроенный fallback-стиль
(тёмная тема в стиле Catppuccin). Чтобы кастомизировать внешний вид,
создайте файл и переопределите нужные селекторы.

> **Миграция с v1 CSS-классов**: в версии 0.2.0 классы были переименованы
> для соответствия модульной архитектуре. Старые имена больше не работают:
> `.window` → `.launcher-window`, `.input` → `.search-entry`,
> `.inner-box` → `.results-list`.
> Строки теперь отрисовываются через `GtkListView` (CSS-ноды `row`), а не `GtkListBox`.

### CSS-классы виджетов

| Класс | Виджет | Назначение |
|-------|--------|------------|
| `.launcher-window` | `GtkWindow` | Окно лаунчера (прозрачный фон, layer-shell overlay) |
| `.outer-box` | `GtkBox` (vertical) | Внешний контейнер — видимая «карточка» лаунчера |
| `.search-entry` | `GtkEntry` | Поле ввода поискового запроса |
| `.results-list` | `GtkListView` | Список результатов поиска (на базе `gio::ListStore`) |
| `.app-icon` | `GtkImage` | Иконка приложения в строке результата |
| `.app-name` | `GtkLabel` | Название приложения в строке результата |

### CSS-селекторы строк

Строки списка — это CSS-ноды `row` внутри `GtkListView`. В GTK4 CSS они выбираются так:

```css
/* Все строки */
.results-list row { … }

/* Выбранная (подсвеченная) строка */
.results-list row:selected { … }
```

### Важные свойства

#### `overflow` — НЕ ПОДДЕРЖИВАЕТСЯ в GTK4 CSS

Свойство `overflow` (включая `overflow: visible`) **не поддерживается** в GTK4 CSS —
это Web-CSS свойство. GTK4 проигнорирует его с предупреждением в лог:
`Theme parser error: No property named "overflow"`.

Для теней, трансформаций и вылетающих элементов используйте отступы окна,
заданные через `window.set_margin()` в коде (левый/правый margin = 60px).

### Базовый пример стиля (светлая тема)

```css
.launcher-window {
    background: transparent;
}

.outer-box {
    background: rgba(255, 255, 255, 0.94);
    border-radius: 16px;
    padding: 16px;
    box-shadow: 0 4px 32px rgba(0, 0, 0, 0.15);
    border: 1px solid rgba(0, 0, 0, 0.08);
}

.search-entry {
    font-size: 16px;
    padding: 10px 16px;
    border-radius: 12px;
    background: rgba(0, 0, 0, 0.04);
    color: #1e1e2e;
    border: 1px solid rgba(0, 0, 0, 0.1);
    margin-bottom: 8px;
    caret-color: #1e1e2e;
}

.search-entry:focus {
    border-color: #7c3aed;
    background: rgba(0, 0, 0, 0.02);
    box-shadow: 0 0 0 3px rgba(124, 58, 237, 0.15);
}

.results-list {
    background: transparent;
}

.results-list row {
    padding: 8px 12px;
    border-radius: 8px;
    color: #1e1e2e;
    font-size: 15px;
}

.results-list row:selected {
    background: rgba(124, 58, 237, 0.12);
    color: #7c3aed;
    font-weight: 600;
}

.app-icon {
    /* Иконки скрыты */
    display: none;
}

.app-name {
    font-weight: 500;
}
```

### Расширенный пример (тёмная тема с иконками)

```css
.launcher-window {
    background: transparent;
}

.outer-box {
    background: rgba(24, 24, 37, 0.96);
    border-radius: 20px;
    padding: 20px;
    box-shadow:
        0 0 0 1px rgba(255, 255, 255, 0.06),
        0 8px 48px rgba(0, 0, 0, 0.5);
}

.search-entry {
    font-size: 17px;
    padding: 12px 18px;
    border-radius: 14px;
    background: rgba(255, 255, 255, 0.06);
    color: #cdd6f4;
    border: 1px solid rgba(255, 255, 255, 0.08);
    margin-bottom: 10px;
    caret-color: #89b4fa;
    transition: border-color 0.15s ease, background 0.15s ease;
}

.search-entry:focus {
    border-color: #89b4fa;
    background: rgba(255, 255, 255, 0.10);
    box-shadow: 0 0 0 3px rgba(137, 180, 250, 0.15);
}

.results-list {
    background: transparent;
}

.results-list row {
    padding: 10px 14px;
    border-radius: 10px;
    color: #bac2de;
    font-size: 15px;
    transition: background 0.12s ease;
}

.results-list row:selected {
    background: rgba(137, 180, 250, 0.15);
    color: #ffffff;
}

.app-icon {
    /* Иконки показываются */
    /* display: none; */ /* раскомментируйте, чтобы скрыть */
    margin-right: 10px;
    min-width: 24px;
    min-height: 24px;
}

.app-name {
    font-weight: 500;
}

/* Анимация появления/исчезновения (если поддерживается композитором) */
.results-list row {
    opacity: 1;
}

.results-list row:drop(active) {
    /* стиль при DnD (если добавите) */
}
```

### CSS-трюки

#### Тень, выходящая за границы окна

Поскольку `.launcher-window` прозрачный, а `.outer-box` — это «карточка»,
тень на `.outer-box` будет отрисовываться корректно:

```css
.outer-box {
    box-shadow: 0 0 60px rgba(0, 0, 0, 0.5);
    margin: 30px; /* даём место тени, если нужно */
}
```

Горизонтальное центрирование окна настраивается через `halign` в коде
(root_box → `halign=Fill`, outer_box → `hexpand=true`).
Внешние отступы окна задаются CSS-свойством `margin` на `.launcher-window`
или `.outer-box`.

#### Анимация появления при фокусе

```css
.search-entry {
    transition: border-color 0.2s ease, box-shadow 0.2s ease;
}

.search-entry:focus {
    border-color: #cba6f7;
    box-shadow: 0 0 0 4px rgba(203, 166, 247, 0.2);
}
```

#### Полупрозрачный blur-эффект (если композитор поддерживает)

```css
.outer-box {
    background: rgba(30, 30, 46, 0.75);
    /* Требуется композитор с blur (Hyprland, sway с эффектами) */
}
```

---

## Горячие клавиши

### Клавиши, встроенные в лаунчер (не настраиваются)

| Клавиша | Действие | Контекст |
|---------|----------|----------|
| **Буквы/цифры** | Ввод поискового запроса | Поле ввода |
| **Escape** | Закрыть лаунчер | Поле ввода |
| **↓ / ↑** | Переместить выделение по списку результатов | Поле ввода |
| **Двойной клик по строке** | Запустить выбранное приложение (без префикса) | Список |

### Бинды запуска (настраиваются через `[[bindings]]`)

Все бинды запуска проверяются **по порядку** — используется первый совпавший.
По умолчанию (если секция `[[bindings]]` отсутствует):

| Комбинация | Действие |
|------------|----------|
| **Enter** | Запустить приложение как `sh -c '<exec>'` |
| **Ctrl+Enter** | Запустить приложение как `sh -c '<exec>'` (без префикса) |

Пример кастомных биндов:

```toml
[[bindings]]
key = "Return"
# без префикса → sh -c '<exec>'

[[bindings]]
key = "Ctrl+Return"
prefix = "proxychains"
# → sh -c 'proxychains <exec>'

[[bindings]]
key = "Shift+Return"
prefix = "foot --"
# → sh -c 'foot -- <exec>'
```

### Логика выбора приложения для запуска

1. Если список результатов видим и есть выделенная строка — запускается
   соответствующее приложение
2. Если список видим, но ничего не выделено — запускается **первый**
   результат
3. Если список скрыт (нет результатов) — запускается **первый** результат
   последнего поиска

---

## Архитектура и IPC

### Общая схема

```
┌──────────────┐     UNIX socket      ┌──────────────┐
│ hiren-client │ ◄──────────────────► │ hiren-daemon │
│   (GTK4)     │   /tmp/hiren.socket  │   (tokio)    │
└──────────────┘                      └──────┬───────┘
                                             │
                                    ┌────────┴────────┐
                                    │  файловая система │
                                    │  .desktop files   │
                                    └──────────────────┘
```

### Протокол (фрейминг)

Каждое сообщение — это **length-prefixed JSON**:

```
┌──────────────────┬─────────────────────────┐
│  4 байта (LE)    │       N байт            │
│  длина JSON      │   JSON-тело (UTF-8)     │
└──────────────────┴─────────────────────────┘
```

Максимальный размер фрейма: **10 МиБ**.

### Типы сообщений

#### RequestSearch

Клиент → Демон. Содержит поисковую строку.

```json
{"RequestSearch": "firefox"}
```

#### ResponseApps

Демон → Клиент. Содержит отфильтрованный и отсортированный список.

```json
{"ResponseApps": [
  {
    "id": "firefox",
    "name": "Firefox",
    "exec": "/usr/bin/firefox",
    "description": "Web Browser"
  }
]}
```

### Алгоритм поиска

Используется крейт `fuzzy-matcher` с движком **skim** (SkimMatcherV2):

- Поиск **без учёта регистра**
- Сопоставление по полю `name` (человекочитаемое название)
- Сортировка по **релевантности** (score skim), от лучшего к худшему
- При **пустом** запросе возвращаются **все** приложения
- Не-совпадающие приложения исключаются из результата

---

## Парсинг .desktop-файлов

### Сканируемые директории

| Директория | Приоритет |
|------------|-----------|
| `/usr/share/applications` | Системные — низкий |
| `~/.local/share/applications` | Пользовательские — **высокий** (перезаписывают системные) |

### Извлекаемые поля

| Поле `.desktop` | Поле `AppEntry` | Примечание |
|-----------------|-----------------|------------|
| `Name` | `name` | Только непереведённое значение (без `[lang]`) |
| `Exec` | `exec` | Очищается от field-кодов (`%f`, `%u`, etc.) |
| `Comment` | `description` | Только непереведённое; `None` если пусто |
| Имя файла (stem) | `id` | Например, `firefox` из `firefox.desktop` |

### Фильтрация

Приложения **пропускаются** (не попадают в кэш), если:

- `NoDisplay=true`
- `Hidden=true`
- Файл не содержит секции `[Desktop Entry]`
- Отсутствует обязательное поле `Name` или `Exec`

### Очистка Exec

Field-коды, удаляемые из строки `Exec`:

| Код | Значение | Код | Значение |
|-----|----------|-----|----------|
| `%f` | Один файл | `%u` | Один URL |
| `%F` | Несколько файлов | `%U` | Несколько URL |
| `%d` | Директория | `%D` | Несколько директорий |
| `%n` | Один файл (устар.) | `%N` | Несколько файлов (устар.) |
| `%i` | Иконка | `%c` | Переведённое имя |
| `%k` | Путь к .desktop | `%v` | Устройство |
| `%m` | Мини-иконка | `%%` | Литерал `%` |

### Дедупликация

Приложения идентифицируются по `id` (stem имени файла). Если приложение
присутствует и в системной (`/usr/share/applications/foo.desktop`),
и в пользовательской (`~/.local/share/applications/foo.desktop`) директории,
**пользовательская версия побеждает**.

### Автообновление кэша

Демон отслеживает изменения в сканируемых директориях через `inotify`
(крейт `notify`). При создании, изменении или удалении любого `.desktop`
файла кэш **пересобирается** после дебаунса **500 мс** (чтобы не
реагировать на каждый чих текстового редактора).

---

## Решение проблем

### «Failed to connect to daemon at /tmp/hiren.socket»

Демон не запущен. Запустите:

```bash
RUST_LOG=debug ./target/release/hiren-daemon
```

### «IPC error: …»

1. Проверьте, что демон запущен и слушает сокет:
   ```bash
   ls -la /tmp/hiren.socket
   ```
2. Проверьте права на `/tmp`:
   ```bash
   ls -ld /tmp
   ```
3. Если демон упал — удалите stale socket:
   ```bash
   rm /tmp/hiren.socket
   ```

### Окно не появляется / не захватывает клавиатуру

1. Убедитесь, что вы запускаете клиент **под Wayland** (не X11/XWayland)
2. Проверьте, что `gtk4-layer-shell` установлен в системе
3. Некоторые композиторы требуют настройку для layer-shell окон.
   **Hyprland**: убедитесь, что layer-rule не блокирует окно
4. Запустите с `WAYLAND_DEBUG=1` для диагностики протокола:
   ```bash
   WAYLAND_DEBUG=1 ./target/release/hiren-client 2>&1 | grep layer
   ```

### Стили не применяются

1. Проверьте, что файл лежит по правильному пути:
   ```bash
   ls -la ~/.config/hiren/style.css
   ```
2. Проверьте синтаксис CSS (нет ошибок парсинга GTK)
3. При старте клиент пишет в stderr: `[hiren-client] Loaded user CSS` —
   если этой строки нет, файл не найден
4. Минимальный тестовый CSS для проверки:
   ```css
   .launcher-window { background: red; }
   ```

### Приложения не запускаются / странное поведение Exec

1. `Exec` запускается через `sh -c '...'` — это обрабатывает quoting и
   переменные окружения в .desktop-файлах
2. Если в `Exec` используются специальные символы, проверьте экранирование
   в исходном `.desktop`-файле
3. Field-коды (`%f`, `%u`) удаляются — приложение запускается без аргументов

### Сборка не удаётся / ошибки компиляции

1. **Минимальная версия Rust**: 1.85+ (edition 2024 в зависимостях)
   ```bash
   rustup update stable
   ```
2. **Системные зависимости** (см. [Быстрый старт](#быстрый-старт))
3. Очистите кэш Cargo:
   ```bash
   cargo clean && cargo build
   ```
