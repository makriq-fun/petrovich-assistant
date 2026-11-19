# Конфигурация

Файл по умолчанию — `/data/petrovich/config.toml`, секреты —
`/data/petrovich/secrets.env` (права `0600`). Неизвестные ключи запрещены:
опечатка останавливает загрузку, а не оставляет небезопасное значение.

Пример: [config/config.example.toml](../config/config.example.toml).

## `[daemon]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `log_level` | `info` | фильтр `tracing`, можно переопределить `RUST_LOG` |
| `state_dir` | `/data/petrovich/state` | состояние/журналы |
| `pid_file` | `/run/petrovichd.pid` | pid демона |
| `secrets_file` | `/data/petrovich/secrets.env` | секреты, `0600` |
| `run_gid` | нет | дополнительный GID, например группа прокси |
| `timezone` | системный | `Europe/Moscow` или `MSK-3`, если zoneinfo нет |

## `[audio]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `capture_pcm` | `default` | ALSA-захват прошивки |
| `playback_pcm` | `default` | ALSA-вывод (softvol Master) |
| `capture_rate` | `48000` | частота захвата |
| `capture_channels` | `6` | всего каналов TDM |
| `mic_channels` | `[0,1,2,3]` | микрофоны |
| `reference_channels` | `[4,5]` | аппаратный референс усилителя |
| `sample_rate` | `16000` | частота распознавания; должна делить `capture_rate` |
| `playback_rate` | `48000` | частота вывода |
| `period_ms` | `20` | период ALSA |
| `vqe` | `auto` | `auto`, `off`, `required` |
| `vqe_preset` | `yandexmini_2` | пресет прошивки |
| `vqe_models` | `/system/vendor/quasar/vqe_models` | модели VQE |
| `alsa_lib` | `""` | путь к libasound; пусто — поиск загрузчиком |
| `aec` | `true` | собственный NLMS, если VQE нет |
| `denoise` | `true` | шумовой затвор в fallback-тракте |
| `aec_delay_ms` | `0` | компенсация задержки тракта |
| `downmix_before_vqe` | `false` | не используется VQE; оставлен для совместимости |

`vqe = "required"` останавливает запуск, если библиотека прошивки недоступна.
`auto` пишет предупреждение и переходит на собственный DSP.

## `[wakeword]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `enabled` | `true` | локальная активация |
| `model` | `/data/petrovich/models/petrovich.tflite` | openWakeWord-голова «Петрович» |
| `assets` | `/data/petrovich/assets/oww.bin` | мел + эмбеддинги для openWakeWord |
| `threshold` | `0.5` | порог срабатывания |
| `sensitivity` | `0.0` | смещение порога; отрицательное — чувствительнее |
| `refractory_ms` | `2000` | пауза между срабатываниями |
| `gain` | `1` | усиление PCM до детектора (0..64) |

Порог ограничивается `0.05..0.999`. Встроенная «Петрович» — готовая
openWakeWord-голова: манифест не нужен, `assets` обязателен. Для
microWakeWord-модели манифест ссылается на `.tflite` рядом с собой; для
openWakeWord проверяются размеры тензоров.
Модели из каталога openwakeword.com по умолчанию лицензированы только для
personal/non-commercial использования; не добавляйте их в репозиторий без
коммерческой лицензии.
Повреждённый JSON не роняет демон, а даёт понятную ошибку при старте.
Других моделей в релизе нет; свою можно передать через `--wakeword-model`
или `model`.

## `[vad]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `min_speech_ms` | `300` | короче — не отправлять в STT |
| `silence_ms` | `700` | тишина для конца фразы |
| `max_utterance_ms` | `12000` | принудительный конец |
| `energy_margin_db` | `9.0` | порог над шумовой полкой |
| `pre_roll_ms` | `300` | звук до начала речи |
| `absolute_db` | `-18.0` | верхний предел порога |

## `[dialog]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `follow_up_ms` | `5000` | ожидание уточнения после ответа |
| `listen_timeout_ms` | `10000` | ожидание речи после активации |
| `history_turns` | `8` | ходов в истории |
| `history_chars` | `12000` | символов в истории |
| `history_ttl_s` | `900` | простой до очистки |
| `barge_in` | `true` | слово активации или кнопка прерывает ответ |
| `barge_in_voice` | `false` | перебивание голосом; включать только с рабочим AEC |
| `wake_chime` | `true` | звук активации |
| `state_chimes` | `true` | звуки «услышал», «перестал слушать», «готово», «остановлено», «очищено» |
| `response_timeout_ms` | `180000` | предел одного хода вместе с озвучкой |
| `max_tool_rounds` | `2` | раундов вызова инструментов (0..5) |

## `[stt]`, `[llm]`, `[tts]`

Общие поля: `base_url`, `model`, `api_key_env`, `timeout_ms`, `retries`.
У LLM есть `fallback_models`: список моделей того же API, которые берутся по
порядку при 429, 404 и при отказе провайдера разобрать вызов инструмента.
`api_key_envs` — дополнительные переменные с ключами того же провайдера: при
429/401 ключи перебираются до смены модели.
STT/TTS поддерживают `openai`, `fish`, `command`; LLM — OpenAI-совместимый
HTTP. У STT есть `tls_ca_file` для self-hosted HTTPS. Для `command`-
провайдеров — `command` (argv, без оболочки). Полные схемы и примеры —
[providers.md](providers.md).

## `[tools]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `enabled` | `time,timer,alarm,volume,music,emotion,web_fetch,weather,reset_dialog,stop` | белый список |
| `timeout_ms` | `10000` | обычные инструменты |
| `web_timeout_ms` | `15000` | веб и погода |
| `web_max_bytes` | `1048576` | предел ответа |
| `search_url` | `""` | SearXNG JSON; пусто — поиска нет |
| `web_allow_private` | `false` | только отладка: частные адреса и HTTP |
| `weather_url` | `https://api.open-meteo.com/v1/forecast` | endpoint погоды |
| `weather_geocoding_url` | `https://geocoding-api.open-meteo.com/v1/search` | геокодинг для города; пусто — только координаты |
| `weather_latitude` | нет | широта |
| `weather_longitude` | нет | долгота |
| `weather_place` | `""` | название в ответе |
| `sandbox_command` | `[]` | argv песочницы; пусто — `run_code` выключен |
| `sandbox_timeout_ms` | `5000` | предел кода |
| `music_command` | `[]` | команда Spotiya, например `["/data/opt/spotify-connect/spotiya", "--{action}"]`; `{action}` подставляется в argv, иначе действие добавляется последним |

Инструмент доступен, только если он одновременно в `enabled`, настроен и
разрешён. `weather` требует `weather_url`, а без координат — названного
города; `web_search` — `search_url`;
`run_code` — `sandbox_command`; `music` — `music_command`.
Таймеры и будильники живут в памяти демона: перезапуск их сбрасывает.

## `[hardware]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `model` | `auto` | `auto` или `station-mini2` |
| `leds` | `true` | кольцо |
| `clock` | `true` | часы, если панель найдена |
| `buttons` | `true` | evdev и GPIO-мут |
| `mic_mute_gpio` | из профиля | переопределение GPIO |
| `input_device` | из профиля | имя evdev |
| `action_key` | `582` | код кнопки действия |
| `volume_step` | `5` | шаг кнопок и инструмента |
| `led_brightness` | `0.8` | максимум 0..1 |
| `clock_brightness` | `0.35` | максимум 0..1 |
| `auto_brightness` | `true` | подстраивать по датчику света |
| `light_sensor` | `""` | каталог IIO или файл; пусто — автопоиск `ltr308` |
| `night_brightness` | `0.15` | минимальная доля яркости ночью |

## `[privacy]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `check_yandex_connections` | `true` | проверка в диагностике |
| `yandex_hosts` | список доменов | что считать обращением к Яндексу |

## Переопределения

- `PETROVICH_CONFIG` — путь к конфигурации;
- `RUST_LOG` — уровень журнала;
- `PETROVICH_HARDWARE_MODEL` — подмена профиля для тестов/диагностики.

Проверка без запуска:

```bash
petrovichd --config config.toml --check-config
```
