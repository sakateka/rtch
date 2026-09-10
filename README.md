# rtch

[English](README.en.md)

Менеджер терминальных сессий на Rust для Linux. Supervisor обслуживает PTY
дочернего процесса; клиенты подключаются через Unix-сокет. Отключение клиента
или SSH-соединения не завершает процесс. Живой вывод передаётся без фильтрации;
эмуляция терминала и управление панелями не реализуются.

## Сборка и установка

Требуется stable Rust 1.88 или новее.

```sh
cargo build --release --locked
install -Dm755 target/release/rtch ~/.local/bin/rtch
```

Каталог `~/.local/bin` должен входить в `PATH`.

## CLI

```text
rtch [OPTIONS] SESSION [PROGRAM [ARGS...]]
rtch [OPTIONS] COMMAND [COMMAND_OPTIONS] [SESSION] [PROGRAM [ARGS...]]
```

Без `PROGRAM` запускается `$SHELL` (или `/bin/sh`) как login-shell.
Bash читает `~/.profile`, если его не перекрывают `~/.bash_profile` или `~/.bash_login`.
Явные программы сохраняют правила запуска; переподключение не перечитывает профиль.
Опции rtch указываются до `PROGRAM`; последующие аргументы передаются программе.

```sh
rtch new monitor htop
# Detach: Ctrl+\
rtch attach monitor

rtch new dev bash -l
rtch start build make
rtch tail -f build
```

| Команда | Действие |
| --- | --- |
| `rtch SESSION` | Подключение к активной сессии или создание отсутствующей |
| `rtch new SESSION [PROGRAM...]` | Создание с подключением; явный перезапуск ended/stale |
| `rtch start SESSION [PROGRAM...]` | Создание без подключения |
| `rtch run SESSION [PROGRAM...]` | Supervisor на переднем плане; возврат кода завершения программы |
| `rtch attach SESSION` | Подключение к активной сессии |
| `rtch detach SESSION` | Отключение всех клиентов без завершения программы |
| `rtch list` / `ended` | Все сессии / только ended |
| `rtch tail [-f] [-n LINES] SESSION` | Чтение истории; `-f` — отслеживание новых данных |
| `rtch push SESSION` | Передача stdin во вход сессии |
| `rtch clear [SESSION]` | Очистка лога и буфера истории; без имени — текущая сессия |
| `rtch kill [-f] SESSION` | SIGTERM, затем SIGKILL через 5 с; `-f` — сразу SIGKILL |
| `rtch rm SESSION` / `rm -a` | Удаление одной / всех ended/stale сессий вместе с историей |
| `rtch current` | Имя текущей сессии или цепочка вложенных сессий |

`Ctrl+\` отключает текущий клиент. Если приложение перехватывает сочетание,
`rtch detach monitor` выполняется из другого терминала. Содержимое экрана
после отключения может сохраниться. Выход из дочернего процесса завершает сессию.

## Состояния сессии

| Состояние | Значение |
| --- | --- |
| `running` | Supervisor работает, подключённых клиентов нет |
| `attached` | Есть подключённые клиенты |
| `ended` | Сессия завершена; сохранены состояние и/или лог |
| `stale` | Сокет остался, обслуживающий процесс отсутствует |

Завершённые сессии требуют явного перезапуска: `rtch new work`.
`rtch work` и `attach` не перезапускают их автоматически. История доступна
через `tail`; `rtch rm work` удаляет завершённую сессию, `rtch rm -a` —
все ended/stale, сохраняя живые. Состояние и код завершения хранятся
в `.ended` даже без логирования; оставшийся без процесса сокет — `[stale]`.

После перезагрузки ОС сохраняются файлы, но не процессы.
Полная справка: `rtch --help`, `rtch COMMAND --help`.

## Конфигурация и хранение

Каталог сессий по умолчанию — `~/.cache/rtch/`.
Пример подготовки отдельного каталога:

```sh
mkdir -p ~/.config/rtch /mnt/data/rtch
chmod 700 /mnt/data/rtch
```

Файл `~/.config/rtch/config`:

```ini
session_dir = /mnt/data/rtch
quiet = false
log_size = 1m
detach_key = ^\
suspend = true
ansi = true
redraw = winch
clear_mode = none
tail_lines = 10
tail_follow = false
```

Приоритет: CLI → конфиг → defaults. Значения выше соответствуют defaults,
кроме примера `session_dir`.

| Ключ | CLI | Значения |
| --- | --- | --- |
| `session_dir` | Абсолютный `SESSION` | Абсолютный путь без кавычек |
| `quiet` | `-q` | `true/false`, подавление сообщений о состоянии |
| `log_size` | `-C SIZE` | Байты, суффиксы `k/m`; `0` отключает лог; максимум `256m` |
| `detach_key` | `-e KEY`, `-E` | Один байт, `^X`; `none` отключает клавишу |
| `suspend` | `-z` отключает | `true/false`, локальная обработка Ctrl+Z |
| `ansi` | `-t` отключает | `true/false`, ANSI-сброс терминала при отключении |
| `redraw` | `-r MODE` | `none/winch/ctrl_l` |
| `clear_mode` | `-R MODE` | `none/move` |
| `tail_lines` | `tail -n LINES` | Неотрицательное целое |
| `tail_follow` | `tail -f` | `true/false` |

Булевы опции допускают явное переопределение: `--quiet=false`, `--no-detach=false`,
`--no-suspend=false`, `--no-ansi=false`, `tail --follow=false`.
Ошибки и неизвестные ключи в конфиге отклоняются. `force`, `all`, имя сессии
и запускаемая команда в конфиг не входят.

Путь абсолютный, без кавычек; пробелы разрешены, `~` и переменные не
подставляются. Пустые строки и комментарии с `#` игнорируются.
При абсолютном `XDG_CONFIG_HOME` конфиг находится в `$XDG_CONFIG_HOME/rtch/config`.

Каталог должен принадлежать текущему UID и запрещать запись группе и остальным.
Абсолютный путь вместо имени переопределяет каталог, но не остальные настройки;
`list` и `rm -a` используют
настроенный каталог. Суффиксы `.log`/`.ended`, двоеточия и управляющие символы
в именах зарезервированы.

## Bash-дополнение

Текущий shell:

```bash
source <(COMPLETE=bash rtch)
```

Автозагрузка при подключённом `bash-completion`:

```bash
completion_dir="${XDG_DATA_HOME:-$HOME/.local/share}/bash-completion/completions"
mkdir -p "$completion_dir"
COMPLETE=bash rtch > "$completion_dir/rtch"
```

Tab дополняет команды, опции, значения `-r`/`-R`, программы и сессии
(включая имена с пробелами). Для `attach/detach/push/kill` — живые,
для `rm` — ended/stale, для `tail/clear` и создания — все.
Список обновляется на каждом Tab.

## История и ограничения

Лог по умолчанию — 1 MiB: при превышении двойного лимита сохраняется последний
1 MiB. `-C 4m` меняет лимит; `-C 0` оставляет только 128 KiB истории в памяти
и файл состояния. При повторном показе фильтруются запросы терминалу и
неподдерживаемые управляющие последовательности; живой вывод не меняется.

При отсоединении и `SIGHUP` восстанавливаются настройки терминала;
`SIGKILL` обработать невозможно. До 64 подключений; клиенты с переполненными
очередями отключаются. Логи могут содержать секреты: приватные файлы и проверка
UID не защищают от процессов того же пользователя.

## Проверки

```sh
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
RTCH_IO_BACKEND=poll cargo test --locked
RTCH_IO_BACKEND=uring cargo test --locked
```

`cargo test` запускает и модульные, и интеграционные тесты. Нужны Bash,
стандартные утилиты Linux, Unix-сокеты и `/dev/ptmx`. Тесты используют
временные каталоги и завершают только собственные сессии.

Механизм I/O: `RTCH_IO_BACKEND=auto` (по умолчанию) ожидает события через io_uring
на Linux 5.11+ и переходит на `poll` при недоступности. `uring` требует io_uring;
`poll` включает совместимый режим. Чтение и запись остаются синхронными.
