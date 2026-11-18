# Установка

## Что нужно

- взломанная Яндекс Станция Мини 2 (`yandex,station-mini2`) с root;
- SSH по ключу или ADB с root;
- свободное место на `/data` (бинарники занимают несколько МиБ; рекомендуется
  не меньше 25 МиБ);
- модель активации `wakeword.json` (см. [wakeword-model.md](wakeword-model.md));
- доступ к выбранным STT/LLM/TTS (OpenAI-совместимый API или локальные
  сервисы).

Установщик проверяет модель и прошивку до записи. Для другой модели нужен
`--allow-unknown-model`, но профиль железа всё равно должен существовать в
`jarvis-core::profile`.

## Сборка

```bash
cargo install cross --git https://github.com/cross-rs/cross
scripts/build.sh
```

Результат:

- `dist/jarvisd` — демон ARMv7 (статический musl);
- `dist/jarvisctl` — диагностика;
- `dist/jarvis-station-<версия>-armv7.tar.gz` — выпуск;
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
python3 installer/jarvis_install.py --ssh-host station install \
  --config config.toml --secrets secrets.env \
  --wakeword-model wakeword.json
```

ADB:

```bash
python3 installer/jarvis_install.py --transport adb --serial <серийный> install \
  --config config.toml --secrets secrets.env \
  --wakeword-model wakeword.json
```

Сначала полезен план:

```bash
python3 installer/jarvis_install.py --ssh-host station --dry-run install
```

Порядок действий установщика:

1. проверяет root, модель, прошивку и свободное место;
2. сохраняет `/data/jarvis` и init-скрипты в
   `backups/install-<UTC>.tar.gz`, скачивает копию на ПК, сверяет SHA256;
3. загружает релиз в `releases/<версия>` и проверяет файлы;
4. атомарно переключает `current`;
5. устанавливает `S99jarvis` и шим Quasar: пока существует
   `/data/jarvis/enabled`, штатный стек не запускается;
6. запускает супервизор и ждёт `--check-config` и живой pid;
7. при неудаче переключается на предыдущий релиз; если его нет — снимает
   `enabled` и оставляет Алису выключенной.

Если нет `--wakeword-model`, установщик попробует импортировать
`/data/opt/wakeword/model.json` из прежней лабораторной установки. Если модели
нет, установка не пройдёт health-check.

## Обновление

```bash
scripts/build.sh
python3 installer/jarvis_install.py --ssh-host station upgrade
```

Конфигурация и секреты без явных `--config/--secrets` не перезаписываются.
Старые релизы остаются для отката (установщик не удаляет их автоматически).

## Независимые DNS и NTP

```bash
python3 installer/jarvis_install.py --ssh-host station privacy \
  --dns 1.1.1.1 --dns 9.9.9.9 --ntp 0.pool.ntp.org
# откат:
python3 installer/jarvis_install.py --ssh-host station privacy --revert
```

Команда сохраняет прежние `/etc/resolv.conf` и конфигурацию NTP в
`/data/jarvis/backups/privacy-*`, перемонтирует rootfs на запись и обратно.
DHCP/NetworkManager может перезаписать `resolv.conf` после перезагрузки —
проверьте через `jarvisctl doctor`.

## Проверка

```bash
/data/jarvis/current/jarvisctl --config /data/jarvis/config.toml status
/data/jarvis/current/jarvisctl --config /data/jarvis/config.toml doctor
tail -n 100 /data/jarvis/log/jarvisd.log
```

`doctor` проверяет: профиль, модель, backbone, VQE, `amixer`, кнопки, часы,
GPIO мута, отсутствие `maind`/`quasar_launcher2`, разрешение доменов Яндекса,
место и память. Он не доказывает отсутствие трафика и не заменяет слуховую
проверку.

## Живая проверка (обязательна)

На колонке:

1. «Джарвис» → сигнал активации;
2. вопрос → ответ;
3. уточняющий вопрос без повторной активации;
4. «Джарвис» во время ответа → ответ прерывается;
5. кнопки громкости, действие, аппаратный мут;
6. светодиоды и часы;
7. таймер;
8. перезагрузка: помощник поднимается, Алиса не отвечает.

Без этих проверок функцию нельзя называть рабочей на конкретной ревизии.

## Диагностика

| Симптом | Что смотреть |
|---|---|
| `jarvisd не запустился` | `/data/jarvis/log/jarvisd.log`, `jarvisctl doctor` |
| нет активации | модель/backbone, `wake-test`, порог, усиление, мут |
| нет звука | `amixer`, `aplay`/`speaker-test`, `[audio] playback_pcm` |
| нет ответа LLM | ключ, URL, сертификат, журнал провайдера |
| Quasar всё ещё работает | шим init-скрипта, `ps`, `jarvisctl doctor` |
| мало места | удалить старые `releases/` после успешной проверки |

Точные ограничения: [testing.md](testing.md), [recovery.md](recovery.md).
