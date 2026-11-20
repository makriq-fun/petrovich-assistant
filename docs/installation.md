# Установка

## Что нужно

- взломанная Яндекс Станция Мини 2 (`yandex,station-mini2`) с root;
- SSH по ключу или ADB с root;
- свободное место на `/data` (бинарники занимают несколько МиБ; рекомендуется
  не меньше 25 МиБ);
- доступ к выбранным STT/LLM/TTS (OpenAI-совместимый API или локальные
  сервисы).

Встроенная модель — openWakeWord «Петрович»
(`assets/wakeword/petrovich.tflite`); она активируется словом «Петрович» и не
требует отдельного файла. Свою модель можно передать через
`--wakeword-model` (см. [wakeword-model.md](wakeword-model.md)).

Установщик проверяет модель и прошивку до записи. Для другой модели нужен
`--allow-unknown-model`, но профиль железа всё равно должен существовать в
`petrovich-core::profile`.

## Сборка

```bash
cargo install cross --git https://github.com/cross-rs/cross
scripts/build.sh
```

Результат:

- `dist/petrovichd` — демон ARMv7, динамический glibc 2.23 (совместим с 2.30 на Мини 2);
- `dist/petrovichctl` — диагностика;
- `dist/petrovich-assistant-<версия>-armv7.tar.gz` — выпуск;
- `dist/SHA256SUMS`.

Для проверки на ПК: `TARGET=host scripts/build.sh`.

## Конфигурация и секреты

```bash
cp config/config.example.toml config.toml
cp config/secrets.example.env secrets.env
chmod 600 secrets.env
$EDITOR config.toml secrets.env
```

Минимум:

- `[stt]`, `[llm]`, `[tts]` — провайдеры и ключи;
- `[tools] weather_url` — погода; координаты задают место по умолчанию,
  без них работает поиск по названному городу;
- `[tools] search_api_key_env` — имя переменной с ключом Tavily; без ключа
  `web_search` недоступен;
- `[tools] nightscout_url` — Nightscout для вопроса о сахаре; при закрытом
  чтении `nightscout_token_env` — имя токена в `secrets.env`;
- `[spotify]`/`[modules] spotify` — встроенный Spotify Connect; без логина
  или access token модуль не запускается. Для голосового поиска при входе
  по access token нужен Tavily-ключ или `spotify.client_id` + secret;
- `[tools] sandbox_command` — только если действительно нужен `run_code`;
- `[tools.computer]` — управление домашним ПК; без секции инструмент
  недоступен;
- `[hardware] model = "auto"`.

Секреты не попадают в репозиторий и не печатаются в журнал. Файл с правами
шире `0600` отвергается при загрузке.

## Установка

SSH (ключ root уже настроен):

```bash
python3 installer/petrovich_install.py --ssh-host station install \
  --config config.toml --secrets secrets.env
```

ADB:

```bash
python3 installer/petrovich_install.py --transport adb --serial <серийный> install \
  --config config.toml --secrets secrets.env
```

Своя модель (необязательно):

```bash
python3 installer/petrovich_install.py --ssh-host station install \
  --config config.toml --secrets secrets.env \
  --wakeword-model ./my_wakeword.tflite
```

Сначала полезен план:

```bash
python3 installer/petrovich_install.py --ssh-host station --dry-run install
```

Порядок действий установщика:

1. проверяет root, модель, прошивку и свободное место;
2. сохраняет `/data/petrovich`, прежние `/data/skynet` и `/data/jarvis`, а также init-скрипты в
   `backups/install-<UTC>.tar.gz`, скачивает копию на ПК, сверяет SHA256;
3. если найдена старая установка, переносит конфигурацию, секреты и
   init-скрипты Quasar в `/data/petrovich`; старые модели и релизы не
   импортируются, старая папка не удаляется;
4. загружает релиз в `releases/<версия>` и проверяет файлы;
5. атомарно переключает `current`;
6. устанавливает `S99petrovich` и шим Quasar: пока существует
   `/data/petrovich/enabled`, штатный стек не запускается;
7. запускает супервизор и ждёт `--check-config` и живой pid;
8. при неудаче переключается на предыдущий релиз; если его нет — снимает
   `enabled` и оставляет Алису выключенной;
9. после успешного health-check оставляет текущий релиз, один предыдущий и
   три последних backup, остальные удаляет.

Если `--wakeword-model` не задан, ставится встроенная `petrovich.tflite` из
`assets/wakeword/`, а `[wakeword] model` в конфиге переписывается на неё.
Старые модели из `/data/skynet`, `/data/jarvis` и `/data/opt/wakeword` не
импортируются.

## Управление ПК

ПК должен принимать ограниченный SSH-endpoint: `command="..."` в
`authorized_keys`, закрытый список действий, без PTY и проброса портов.
Файлы для колонки — `dbclient`, приватный ключ и `known_hosts` с ключом хоста
ПК. Установщик умеет положить их сразу:

```bash
python3 installer/petrovich_install.py --ssh-host station install \
  --config config.toml --secrets secrets.env \
  --pc-ssh-bin ./dbclient \
  --pc-key ./client-key \
  --pc-known-hosts ./known_hosts
```

Файлы окажутся в `/data/petrovich/pc/` с правами `0755` у `dbclient` и
`0600` у ключей. Без флагов положите их туда вручную; путь к `dbclient`
задаётся в `[tools.computer] ssh_bin`. Проверка:

```bash
/data/petrovich/current/petrovichctl --config /data/petrovich/config.toml computer-test
```

Добавьте `"computer"` в `[tools] enabled` (в примере он уже есть) и
раскомментируйте `[tools.computer]`.

## Обновление

```bash
scripts/build.sh
python3 installer/petrovich_install.py --ssh-host station upgrade
```

Конфигурация и секреты без явных `--config/--secrets` не перезаписываются.
Старые релизы остаются для отката (установщик не удаляет их автоматически).
`libtensorflowlite_c.so` не входит в архив, но при обновлении копируется из
текущего релиза в новый: без него загрузчик возьмёт старый
`/system/vendor/quasar/libs/libtflite.so` и упадёт на модели openWakeWord.

## Независимые DNS и NTP

```bash
python3 installer/petrovich_install.py --ssh-host station privacy \
  --dns 1.1.1.1 --dns 9.9.9.9 --ntp time.cloudflare.com
# откат:
python3 installer/petrovich_install.py --ssh-host station privacy --revert
```

Команда сохраняет прежние `/etc/resolv.conf` и конфигурацию NTP в
`/data/petrovich/backups/privacy-*`, перемонтирует rootfs на запись и обратно.
DHCP/NetworkManager может перезаписать `resolv.conf` после перезагрузки —
проверьте через `petrovichctl doctor`.

## Hysteria 2 для STT/LLM

Если провайдер блокирует `api.groq.com` по IP, клиент Hysteria 2 поднимает
локальный SOCKS5 только для STT/LLM:

```toml
[stt]
proxy = "socks5h://127.0.0.1:1080"

[llm]
proxy = "socks5h://127.0.0.1:1080"
```

Бинарник и конфиг лежат в `/data/petrovich/opt/hysteria2/` и
`/data/petrovich/hysteria.yaml` (права 600, в репозиторий не попадают).
`/etc/init.d/S98hysteria2` распаковывает сжатый бинарник в `/tmp` и запускает
его до `petrovichd`. Остальной трафик колонки идёт напрямую.

## Проверка

```bash
/data/petrovich/current/petrovichctl --config /data/petrovich/config.toml status
/data/petrovich/current/petrovichctl --config /data/petrovich/config.toml doctor
/data/petrovich/current/petrovichctl --config /data/petrovich/config.toml audio-test 5
tail -n 100 /data/petrovich/log/petrovichd.log
```

`doctor` проверяет: профиль, модель, backbone, VQE, `amixer`, кнопки, часы,
GPIO мута, отсутствие `maind`/`quasar_launcher2`, разрешение доменов Яндекса,
место и память. Он не доказывает отсутствие трафика и не заменяет слуховую
проверку.

## Живая проверка (обязательна)

На колонке:

1. слово активации → сигнал активации;
2. вопрос → ответ;
3. уточняющий вопрос без повторной активации;
4. слово активации во время ответа → ответ прерывается;
5. кнопки громкости, действие, аппаратный мут;
6. светодиоды и часы;
7. таймер;
8. перезагрузка: помощник поднимается, Алиса не отвечает.

Без этих проверок функцию нельзя называть рабочей на конкретной ревизии.

## Диагностика

| Симптом | Что смотреть |
|---|---|
| `petrovichd не запустился` | `/data/petrovich/log/petrovichd.log`, `petrovichctl doctor` |
| нет активации | модель/backbone, `wake-test`, порог, усиление, мут |
| нет звука | `amixer`, `aplay`/`speaker-test`, `[audio] playback_pcm` |
| нет ответа LLM | ключ, URL, сертификат, журнал провайдера |
| Quasar всё ещё работает | шим init-скрипта, `ps`, `petrovichctl doctor` |
| мало места | удалить старые `releases/` после успешной проверки |

Точные ограничения: [testing.md](testing.md), [recovery.md](recovery.md).
