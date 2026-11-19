# Lite Stream Browser

Минималистичная десктопная оболочка для **YouTube** и **Twitch** на **Rust + Tauri v2**.
Без React/Vue и без сборщика фронтенда: верхняя панель — это чистые `HTML/CSS/JS`,
а вкладками и дочерними WebView управляет Rust.

Главная идея — **в памяти живёт только одна вкладка**. При переключении или нажатии
«Сон» дочерний WebView фоновой вкладки уничтожается вместе с процессом рендеринга,
а URL и заголовок остаются в Rust. При возврате WebView создаётся заново с тем же
постоянным профилем, поэтому страница открывается уже авторизованной.

---

## Возможности

| Возможность | Как работает |
|---|---|
| **+ YouTube / + Twitch** | Создают вкладку и активный дочерний WebView (быстрые кнопки на панели и крупные карточки на домашнем экране) |
| **Единый профиль** | Все дочерние WebView используют одну папку `data_directory`, поэтому cookies, логины, подписки и рекомендации общие |
| **Сон вкладки** | Закрывает child WebView, сохраняя URL/заголовок в `AppState` под `Mutex` |
| **Переключение вкладок** | Текущий WebView уничтожается, целевой создаётся лениво из сохранённого URL |
| **Адресная строка** | URL, домен без схемы, `localhost` или поисковый запрос (DuckDuckGo) |
| **Назад / Обновить** | `history.back()` и нативная перезагрузка WebView |
| **Поверх всех окон** | `set_always_on_top` у главного окна |
| **Компактный плеер** | `Ctrl+H` скрывает панель 36 px и растягивает WebView на 100% высоты |
| **Авторизация** | Нативный User-Agent системного WebView2 с актуальным Chromium + постоянный профиль; Edge/WebView2-токены и Client Hints нормализуются до Google Chrome |
| **Импорт сессии Twitch** | Кнопка с ключом вставляет `auth-token` (и при желании `twilight-user`/`persistent`) из обычного браузера прямо в cookie-профиль WebView2 |
| **Расширения WebView2** | Поддерживаются распакованные Chrome/Edge-расширения из постоянной папки `extensions`; установка идёт через профиль WebView2 с дедупликацией |
| **Домашний лаунчер** | Современный тёмный экран с быстрым поиском/переходом, избранным, списком расширений, шпаргалкой хоткеев и статусом профиля/RAM |
| **Попапы** | `target="_blank"` и OAuth-попапы открываются как обычные вкладки в том же профиле |

---

## Как устроена экономия памяти

Главное окно Tauri содержит **основной WebView** (панель вкладок) и **один дочерний
WebView** активной вкладки, который располагается ровно под панелью:

```
┌──────────────────────────────────────────────────────────────┐
│  main WebView (HTML/CSS/JS) — панель 36 px                   │
│  [←] [↻] [+ YouTube] [+ Twitch] [адрес] [вкладки] [🔑] [☾] [⤢] [📌]│
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  child WebView активной вкладки                              │
│  (YouTube / Twitch / любой URL)                              │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

* Панель и домашний экран рендерятся в основном WebView.
* Дочерний WebView создаётся через `Window::add_child` (фича `unstable`).
* Его позиция — `(0, 36 × scale_factor)`, размер — вся оставшаяся область окна.
* При ресайзе окна и смене DPI Rust пересчитывает геометрию (`apply_layout`).
* Пока вкладок нет, отображается домашний экран в основном WebView; при открытии
  любой вкладки он скрывается, а дочерний WebView занимает всё пространство ниже
  панели без наложений.
* В один момент существует **максимум один** дочерний WebView.

Жизненный цикл вкладки:

```
открыть/переключить
        │
        ▼
  ┌─────────────┐   переключение / «Сон»   ┌─────────────┐
  │  Awake      │ ───────────────────────► │  Sleeping   │
  │ child WV    │                          │ нет WV      │
  │ в памяти     │ ◄─────────────────────── │ URL+title   │
  └─────────────┘   клик / «Разбудить»     └─────────────┘
                         add_child(data_directory, saved_url)
```

Состояние лежит в `Arc<Mutex<AppState>>`:

```rust
pub struct AppState {
    pub tabs: Vec<Tab>,              // id, title, url, sleeping, webview_label
    pub active: Option<u64>,
    pub compact: bool,
    pub always_on_top: bool,
    pub data_directory: PathBuf,     // общий профиль всех вкладок
    pub extensions_directory: PathBuf,// распакованные расширения WebView2
}
```

Команды Tauri — `async`, потому что создание WebView на Windows нельзя выполнять
в синхронной команде или обработчике события: WebView2 может заблокироваться.
Обработчик `on_new_window` для этого явно запускает отдельный поток.

---

## Структура проекта

```
lite-stream-browser/
├── Cargo.toml                     # workspace + release-профиль
├── Cargo.lock
├── README.md
├── .gitignore
├── ui/
│   └── index.html                 # панель и домашний лаунчер: HTML + CSS + JS, без фреймворков
└── src-tauri/
    ├── Cargo.toml                 # tauri = { features = ["unstable"] }
    ├── build.rs
    ├── tauri.conf.json            # окно, identifier, withGlobalTauri
    ├── capabilities/
    │   └── default.json           # core:default / core:event:default
    ├── icons/
    │   ├── icon.ico
    │   └── icon.png
    └── src/
        ├── main.rs                # точка входа
        ├── lib.rs                 # Builder, плагин Ctrl+H, окно, пути профиля/расширений
        ├── state.rs               # AppState, Tab, UiState, SharedState
        ├── browser.rs             # add_child, layout, hibernate, UA, установка расширений
        └── commands.rs            # Tauri-команды, URL, Twitch-токен, расширения
```

---

## Требования

* **Windows 10/11** (проект в первую очередь рассчитан на WebView2).
* **Rust** 1.77.2 или новее.
* **Microsoft Edge WebView2 Runtime** — обычно уже установлен в Windows 10/11.
* Node.js и npm **не нужны**: фронтенд статический.
* Tauri CLI нужен только для `cargo tauri dev/build`; для запуска достаточно `cargo run`.

---

## Запуск

Из корня проекта:

```powershell
cargo check          # проверка типов
cargo run            # сборка и запуск приложения
```

Либо из папки `src-tauri`:

```powershell
cd src-tauri
cargo run
```

Через Tauri CLI (опционально):

```powershell
cargo install tauri-cli --version "^2"
cargo tauri dev
cargo tauri build
```

> В `tauri.conf.json` стоит `"bundle": { "active": false }`, поэтому
> `cargo tauri build` соберёт исполняемый файл без установщика. Для MSI/NSIS
> включите `bundle.active`, добавьте полный набор иконок и укажите цели сборки.

Проверено с `tauri 2.12.7`, `tauri-plugin-global-shortcut 2.4.1`, Rust 1.100.0:
`cargo check` завершается успешно.

---

## Авторизация и постоянная сессия

Все дочерние WebView получают одну и ту же папку:

```text
%LOCALAPPDATA%\com.litestream.browser\browser-profile
```

(точный путь показывается на домашнем экране внизу, в строке «Профиль»).

Это стандартная папка пользовательских данных WebView2 (`data_directory`). Рядом
с ней приложение создаёт папку распакованных расширений:

```text
%LOCALAPPDATA%\com.litestream.browser\extensions
```

Её путь тоже отображается на домашнем экране («Расширения»). Первая вкладка
устанавливает найденные расширения в профиль WebView2; на домашнем экране новые
папки могут отображаться как ожидающие, пока не создан child WebView.
Она общая для всех вкладок и сохраняется между запусками, поэтому:

* вход в Google/YouTube и Twitch сохраняется;
* подписки, история просмотров и персональные рекомендации не теряются;
* после «сна» вкладка открывается уже авторизованной;
* cookies и `localStorage` доступны всем вкладкам.

User-Agent **не фиксируется в коде**: используется нативный UA установленного
системного WebView2, поэтому версия Chromium всегда актуальная (140+). Чтобы Google и
Twitch не воспринимали встроенный движок как неподдерживаемый браузер, при старте
каждой вкладки:

* в WebView2 передаётся `--disable-blink-features=AutomationControlled` (вместе с
  дефолтными `--disable-features` от wry);
* сразу после создания дочернего WebView через `with_webview` читается нативный
  `ICoreWebView2Settings2::UserAgent`, из него удаляются `Edg/...`, `Edge/...` и
  любые токены `WebView2`, а результат записывается обратно через `SetUserAgent`.
  Поэтому **сетевой** заголовок `User-Agent`, который проверяет
  `passport.twitch.tv`, тоже выглядит как обычный Chrome с реальной версией
  Chromium;
* `initialization_script` выполняется до скриптов страницы и:
  * скрывает `navigator.webdriver` (возвращает `undefined`);
  * удаляет из `navigator.userAgent` только токены `Edg/...`, `WebView2/...` и
    `Microsoft Edge WebView2`, сохраняя реальную версию `Chrome/<версия>`;
  * заменяет бренды `navigator.userAgentData.brands` на `Google Chrome` +
    `Chromium` с реальной major-версией движка и приводит к ним же
    `getHighEntropyValues()`.

Итоговый UA выглядит как обычный Windows Chrome, но его версия берётся из
установленного WebView2, а не из захардкоженной строки.

**Как войти:** откройте YouTube или Twitch, нажмите «Войти» и авторизуйтесь как в
обычном браузере. Сессия сохранится автоматически.

**Сбросить профиль:** закройте приложение и удалите папку `browser-profile`.

> Google иногда ограничивает вход в сторонних embedded-браузерах. Реалистичный UA
> решает большинство случаев, но политика Google может меняться.

## Вход в Twitch по токену

Форма логина Twitch (`passport.twitch.tv`) дополнительно прогоняет браузер через
integrity-check и даже с чистым сетевым `User-Agent` может отвечать «Ваш браузер пока
не поддерживается». Обходить проверку не нужно: сессию можно один раз перенести из
обычного браузера.

1. Войдите в Twitch в Chrome/Firefox/Edge.
2. Откройте DevTools → **Application → Cookies → `https://www.twitch.tv`** и скопируйте
   значение cookie `auth-token` (или всю строку вида
   `auth-token=...; twilight-user=...; persistent=...`).
3. В приложении нажмите кнопку с ключом на верхней панели
   (tooltip «Вставить auth-token Twitch из обычного браузера»).
4. Вставьте значение в строку `Twitch auth-token` и нажмите «Войти» (или `Enter`).
   `Esc` закрывает строку без изменений.

Что происходит дальше:

* команда `import_twitch_token` разбирает ввод (голое значение токена или строка
  cookie) и оставляет только `auth-token`, `twilight-user`, `persistent`;
* если активна не Twitch-вкладка, приложение находит существующую вкладку Twitch или
  открывает `https://www.twitch.tv`;
* через `with_webview` → `ICoreWebView2_2::CookieManager` создаются cookie для
  `.twitch.tv`, путь `/`, `IsSecure = true`, `IsHttpOnly = false`,
  `SameSite = NONE` и сроком действия +1 год;
* после подтверждения записи вкладка Twitch перезагружается и открывается уже
  авторизованной.

Cookie живут в постоянном `data_directory` (`browser-profile`), поэтому импорт нужен
только один раз — при следующих запусках сессия восстанавливается сама. Если токен
недействителен, Twitch сам удалит cookie при загрузке страницы — это ожидаемо.

Строка ввода намеренно встроена в 36 px панель, а не сделана модальным окном:
дочерний WebView рендерится поверх основного, и обычный диалог был бы скрыт.

---

## Расширения WebView2

Приложение включает поддержку распакованных Chrome/Edge-расширений в дочерних
WebView2:

* при создании вкладки `WebviewBuilder` получает
  `.browser_extensions_enabled(true)` (это включает
  `ICoreWebView2EnvironmentOptions::SetAreBrowserExtensionsEnabled(true)` в wry);
* постоянная папка расширений: `%LOCALAPPDATA%\com.litestream.browser\extensions`;
* сканируются только **подпапки верхнего уровня**, в которых есть `manifest.json`;
* установка выполняется вручную через
  `ICoreWebView2Profile7::AddBrowserExtensionAsync` (через
  `profile.cast::<ICoreWebView2Profile7>()?`) после `add_child`, чтобы избежать
  дублей и лишних попыток;
* для дедупликации используется реестр `<extensions>\.installed.json`
  (канонический путь папки + id/имя); перед установкой он сверяется с
  `GetBrowserExtensionsAsync`;
* если были добавлены новые расширения, активная вкладка перезагружается один
  раз — WebView2 применяет их при следующей навигации.

Команды Tauri:

* `open_extensions_folder` — создаёт папку `extensions` (если её нет) и
  открывает её в Проводнике Windows (`explorer.exe`);
* `list_extensions` — возвращает список папок-расширений (`name`, `version`,
  `path`, `id`, `status`) для домашнего экрана. Если child WebView уже создан,
  список объединяется с живыми данными профиля (`active` / `disabled`).

Как добавить `uBlock Origin`, `7TV`, `SponsorBlock`:

1. Скачайте **распакованную** версию расширения (папку с `manifest.json`), а не
   `.crx`/`.zip`.
2. Нажмите кнопку «🧩» на верхней панели 36 px или «Открыть папку расширений» на
   домашнем экране.
3. Скопируйте папку расширения в `extensions` (каждое расширение — отдельная
   подпапка верхнего уровня).
4. Откройте новую вкладку (или перезапустите приложение). Новые расширения
   установятся в профиль при создании вкладки.

Ограничения: расширения устанавливаются только в профиле дочерних вкладок
(основной WebView панели использует отдельный data dir и расширения в нём
отключены). UI-попапы расширений, магазин Chrome Web Store и автообновления не
поддерживаются — только локальные unpacked-папки.

---

## Управление

| Действие | Элемент / клавиша |
|---|---|
| Новая вкладка YouTube | кнопка **+ YouTube** на панели или карточка на домашнем экране |
| Новая вкладка Twitch | кнопка **+ Twitch** на панели или карточка на домашнем экране |
| Быстрый переход / поиск с домашнего экрана | центральная строка — URL, домен или запрос + `Enter` |
| Быстрый канал / избранное | карточка в секции «Быстрые каналы» (добавление/удаление, хранится в `localStorage`) |
| Открыть папку расширений | кнопка «🧩» на панели 36 px или «Открыть папку расширений» на домашнем экране |
| Импорт сессии Twitch | кнопка с ключом 🔑, затем вставка `auth-token` и `Enter` |
| Открыть URL или найти в интернете | адресная строка + `Enter` |
| Назад | кнопка `←` (или `Alt+←`, когда фокус на панели) |
| Обновить | кнопка `↻` (или `Ctrl+R` / `F5`, когда фокус на панели) |
| Фокус в адресную строку | `Ctrl+L`, когда фокус на панели |
| Усыпить активную вкладку | кнопка `☾` |
| Разбудить спящую вкладку | кнопка `☀` или клик по её вкладке |
| Показать/скрыть панель 36 px | **`Ctrl+H`** или кнопка `⤢` |
| Поверх всех окон | кнопка `📌` |
| Закрыть вкладку | `×` на вкладке |
| Переключить вкладку | клик по вкладке; `Enter`/`Space` с клавиатуры |
| Закрыть вкладку с клавиатуры | `Delete` / `Backspace` на вкладке |

`Ctrl+H` регистрируется как глобальный hotkey **только пока окно приложения в фокусе**
(при потере фокуса снимается). Это позволяет использовать его, когда фокус находится
внутри дочернего WebView YouTube/Twitch, и не отбирать сочетание у других программ.

---

## Где что менять

| Что | Файл |
|---|---|
| Высота панели | `PANEL_HEIGHT` в `src-tauri/src/browser.rs` и `--bar` в `ui/index.html` |
| Аргументы WebView2, сетевой User-Agent и JS-нормализация Client Hints | `BROWSER_ARGS`, `apply_clean_user_agent`, `INITIALIZATION_SCRIPT` в `src-tauri/src/browser.rs` |
| Расширения: сканирование, установка, дедупликация, список | `sync_extensions`, `add_extensions`, `collect_extensions` в `src-tauri/src/browser.rs` |
| Команды расширений | `open_extensions_folder`, `list_extensions` в `src-tauri/src/commands.rs` и регистрация в `src-tauri/src/lib.rs` |
| Импорт Twitch-сессии, домен/флаги/срок cookie | `import_twitch_token` в `src-tauri/src/commands.rs`, `install_twitch_cookies`/`set_twitch_cookies` в `src-tauri/src/browser.rs` |
| Домашний экран, избранное, оформление панели | `ui/index.html` (карточки, избранное в `localStorage`, панель расширений, хоткеи) |
| Папка профиля и папка расширений | `app_local_data_dir()/browser-profile`, `app_local_data_dir()/extensions` в `src-tauri/src/lib.rs` |
| Права Tauri | `src-tauri/capabilities/default.json` |
| Размеры/заголовок окна | `src-tauri/tauri.conf.json` |

---

## Технические детали

* `tauri = { features = ["unstable"] }` — обязательное условие для
  `WebviewBuilder` / `Window::add_child`.
* `additional_browser_args` заменяет дефолтные аргументы wry, поэтому
  `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` продублирован
  рядом с `--disable-blink-features=AutomationControlled`.
* `apply_clean_user_agent` через `with_webview` получает
  `ICoreWebView2Controller -> CoreWebView2 -> Settings -> ICoreWebView2Settings2`,
  читает нативный `UserAgent`, вырезает `Edg/...`/`WebView2` и записывает строку
  обратно через `SetUserAgent`. Это очищает и сетевой HTTP-заголовок, и
  `navigator.userAgent`, сохраняя версию Chromium установленного WebView2.
* `initialization_script` дополнительно скрывает `navigator.webdriver` и
  нормализует Client Hints (`navigator.userAgentData.brands`) до
  `Google Chrome`/`Chromium`.
* `WebviewBuilder::browser_extensions_enabled(true)` включает поддержку расширений
  в окружении WebView2. Сами распакованные расширения устанавливаются вручную в
  `create_child` через `ICoreWebView2Profile7::AddBrowserExtension` (канал
  `mpsc` + `ProfileAddBrowserExtensionCompletedHandler`, т.к. обработчики
  выполняются на UI-потоке WebView2). Список для UI берётся из сканера папки и,
  при наличии живого WebView, из `GetBrowserExtensions`.
* Для COM-вызовов используются `webview2-com` и `windows-core` — обе библиотеки
  уже присутствуют в графе зависимостей wry/Tauri, новых тяжёлых крейтов нет.
* `install_twitch_cookies` отправляет запись в `with_webview` и ждёт подтверждения
  через `std::sync::mpsc` (таймаут 15 с), поэтому `reload()` вызывается только после
  того, как `AddOrUpdateCookie` реально завершился. `AddOrUpdateCookie` синхронный,
  `CreateCookie` — нет; цепочка интерфейсов:
  `ICoreWebView2 -> cast::<ICoreWebView2_2>() -> CookieManager()`.
* Значение cookie из инпута не логируется и не сохраняется в `AppState` — оно сразу
  уходит в WebView2, а дальше живёт в зашифрованном cookie-хранилище профиля
  (`...\browser-profile\EBWebView\Default\Network\Cookies`).
* `on_page_load` сохраняет актуальный URL, `on_document_title_changed` — реальный
  заголовок страницы. Оба колбэка обновляют `AppState` и отправляют событие
  `state-changed` в основной WebView.
* Каждому WebView выдаётся уникальный label вида `tab-<id>-<generation>`.
  Это защищает от коллизии, если WebView2 ещё асинхронно уничтожает старый view.
* Все обращения к WebView выполняются **без удержания мьютекса**: блокировки
  снимаются до `add_child`, `close`, `set_size` и `navigate`, поэтому колбэки
  заголовка/загрузки не могут вызвать дедлок.
* Координаты дочернего WebView считаются в физических пикселях:
  `36 * scale_factor`, что сохраняет высоту панели на HiDPI-экранах.
* Одновременно живёт только один дочерний WebView — это главный механизм
  экономии RAM/CPU.

---

## Известные ограничения

* Интерфейс панели и домашнего экрана на русском языке.
* Поддерживаются только **распакованные** (unpacked) расширения из локальной папки
  `extensions`; Chrome Web Store, `.crx`, UI-попапы расширений и автообновления
  не реализованы. Расширения подключаются к профилю дочерних вкладок.
* Новые папки расширений устанавливаются при создании вкладки (когда WebView2 уже
  создал профиль), поэтому до первой вкладки их статус на домашнем экране может
  быть «ожидает».
* Нет закладок/истории как отдельной подсистемы (кроме «Быстрых каналов» на
  домашнем экране), нет менеджера загрузок.
* Google может периодически требовать подтверждение входа в embedded-браузере.
* DRM-видео (например, некоторые платные сервисы) может не поддерживаться WebView2.
* Профиль общий для всех вкладок: выход из аккаунта в одной вкладке затрагивает все.
* `bundle.active = false` — установщики нужно настраивать отдельно. Релизный
  исполняемый файл собирается как `target\release\lite-stream-browser.exe`.
