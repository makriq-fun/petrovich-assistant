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
- `[tools] weather_latitude/longitude` — погода;
- `[tools] search_url` — SearXNG, если нужен поиск;
- `[tools] sandbox_command` — только если действительно нужен `run_code`;
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
   `enabled` и оставляет Алису выключенной.

Если `--wakeword-model` не задан, ставится встроенная `petrovich.tflite` из
`assets/wakeword/`, а `[wakeword] model` в конфиге переписывается на неё.
Старые модели из `/data/skynet`, `/data/jarvis` и `/data/opt/wakeword` не
импортируются.

## Обновление

```bash
scripts/build.sh
python3 installer/petrovich_install.py --ssh-host station upgrade
```

Конфигурация и секреты без явных `--config/--secrets` не перезаписываются.
Старые релизы остаются для отката (установщик не удаляет их автоматически).

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
