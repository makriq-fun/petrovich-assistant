# Конфигурация

Файл по умолчанию — `/data/petrovich/config.toml`, секреты —
`/data/petrovich/secrets.env` (права `0600`). Неизвестные ключи запрещены:
опечатка останавливает загрузку, а не оставляет небезопасное значение.

Пример: [config/config.example.toml](../config/config.example.toml).

## `[network]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `proxy` | `""` | общий прокси для STT, LLM, TTS, Tavily, `web_fetch`, погоды, Nightscout, Spotify API и librespot |

Это SOCKS5/SOCKS5h/HTTP(S), например `socks5h://127.0.0.1:1080`. Явный
`proxy` у STT, LLM, TTS или Spotify имеет приоритет над общим. Все компоненты,
включая librespot, поддерживают SOCKS5h: для librespot демон поднимает
локальный HTTP CONNECT-мост на loopback. Прокси не меняет системные настройки
и применяется только внутри демона.

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
| `mic_gain` | `0` | усиление PDM-микрофона карты 0, %; 0 — не менять |

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

## `[commands]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `enabled` | `true` | быстрые команды без слова активации |
| `model` | `/system/vendor/quasar/spotter_models/command` | каталог command spotter из прошивки |

Модель — штатный `chen14`-споттер Яндекса: 16 кГц mono, 20 мс на вызов,
28 выходов. Она уже лежит в прошивке Станции Мини 2 и не входит в репозиторий.
Если каталога нет, демон пишет предупреждение и работает без быстрых команд.

Команды: `дальше`, `назад`, `играй`, `продолжи`, `следующий` — музыка;
`громче`, `тише` — громкость; `пауза`, `стоп`, `хватит` — глобальная остановка.
Глобальная остановка действует только на передний план: активный ход
ассистента (`Thinking`) останавливает ассистента, иначе звучащая музыка —
музыку, и только при пассивном `Listening` без музыки — ожидание ассистента.
Остальные слова модели (`выше`, `ниже`, `включи свет`, `домой` и т.п.)
игнорируются. Пока звучит TTS или сигнал, музыкальные команды и громкость
пропускаются, чтобы озвучка не переключала трек; «пауза/стоп/хватит»
работают всегда.

## `[vad]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `min_speech_ms` | `300` | короче — не отправлять в STT |
| `silence_ms` | `2200` | тишина для конца длинной фразы |
| `short_silence_ms` | `1400` | тишина для короткой команды |
| `short_utterance_ms` | `4000` | до этой длины считается короткой |
| `resume_pause_ms` | `1100` | пауза, которую всегда пережидаем |
| `max_utterance_ms` | `45000` | принудительный конец |
| `energy_margin_db` | `6.0` | порог над шумовой полкой |
| `pre_roll_ms` | `300` | звук до начала речи |
| `absolute_db` | `-55.0` | нижняя граница порога для тихой речи |

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
| `max_tool_rounds` | `20` | раундов вызова инструментов (0..20) |

## `[memory]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `enabled` | `true` | долговременная память и инструменты `remember`/`forget` |
| `path` | `<state_dir>/memory.json` | файл фактов, права `0600` |
| `max_entries` | `200` | предел числа фактов (1..1000) |
| `max_chars` | `8000` | предел суммарной длины |
| `prompt_chars` | `2000` | сколько символов памяти попадает в системный промпт |

Память хранит короткие факты о пользователе: имя, город, предпочтения,
договорённости. `remember` сохраняет факт, `forget` удаляет совпадения по
подстроке, пустой аргумент очищает память. Факты не сбрасываются
`reset_dialog` и переживают перезапуск; в системный промпт добавляются самые
свежие из них в пределах `prompt_chars`. Это не история диалога: история
по-прежнему живёт в памяти процесса и ограничена `[dialog]`.

## `[stt]`, `[llm]`, `[tts]`

Общие поля: `base_url`, `model`, `api_key_env`, `timeout_ms`, `retries`.
У LLM есть `fallback_models`: список моделей того же API, которые берутся по
порядку при 429, 404 и при отказе провайдера разобрать вызов инструмента.
`api_key_envs` — дополнительные переменные с ключами того же провайдера: при
429/401 ключи перебираются до смены модели.
`proxy` у STT, LLM и TTS — SOCKS5/HTTP только для этих запросов, например
`socks5h://127.0.0.1:1080` (Hysteria 2). Пусто — берётся общий
`[network] proxy`. Для librespot SOCKS5 автоматически поднимается локальным
HTTP CONNECT-мостом в процессе `petrovichd`; отдельный процесс не нужен.
Прокси не меняет системный маршрут и не требует прав на сеть. STT/TTS поддерживают `openai`, `fish`,
`chat_audio`, `command`; LLM —
OpenAI-совместимый HTTP. `chat_audio` отправляет WAV в мультимодальную
chat-модель (например, Gemini через VoidAI), когда отдельного Whisper-эндпоинта
у провайдера нет. У STT есть `tls_ca_file` для self-hosted HTTPS. Для `command`-
провайдеров — `command` (argv, без оболочки). Полные схемы и примеры —
[providers.md](providers.md).

## `[tools]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `enabled` | `time,timer,alarm,volume,music,youtube,web_search,emotion,web_fetch,weather,sugar,remember,forget,reset_dialog,stop` | белый список |
| `timeout_ms` | `10000` | обычные инструменты |
| `web_timeout_ms` | `15000` | веб, погода и сахар |
| `web_max_bytes` | `1048576` | предел ответа |
| `search_proxy` | `""` | прокси только для Tavily; пусто — общий `[network] proxy` |
| `search_api_key_env` | `""` | переменная secrets.env с ключом Tavily; пусто — `web_search` выключен |
| `web_allow_private` | `false` | только отладка: частные адреса и HTTP |
| `weather_url` | `https://api.open-meteo.com/v1/forecast` | endpoint погоды |
| `weather_geocoding_url` | `https://geocoding-api.open-meteo.com/v1/search` | геокодинг для города; пусто — только координаты |
| `weather_latitude` | нет | широта |
| `weather_longitude` | нет | долгота |
| `weather_place` | `""` | название в ответе |
| `nightscout_url` | `""` | базовый URL Nightscout (`https://ns.example`); пусто — `sugar` выключен |
| `nightscout_token_env` | `""` | переменная secrets.env с access token Nightscout; пусто — без токена |
| `sandbox_command` | `[]` | argv песочницы; пусто — `run_code` выключен |
| `sandbox_timeout_ms` | `5000` | предел кода |
| `music_command` | `[]` | устаревший внешний backend; при включённом Spotify не используется |
| `youtube_proxy` | `""` | внешний прокси для `youtube`; пусто — встроенный ByeDPI |

Инструмент доступен, только если он одновременно в `enabled`, настроен и
разрешён. `weather` требует `weather_url`, а без координат — названного
города; `web_search` — ключ Tavily в `search_api_key_env`,
`search_proxy` — отдельный прокси для него (иначе общий); `run_code` — `sandbox_command`;
`music` — Spotify или `music_command`; `youtube` — `youtube_proxy` или
встроенный ByeDPI; `remember`/`forget` — `memory.enabled`;
`sugar` — `nightscout_url`.
`sugar` читает `/api/v1/entries/sgv.json` (только чтение), переводит mg/dL в
ммоль/л и отдаёт значение, тренд, изменение за ~15 минут и диапазон за
последние часы; `hours` в вызове задаёт глубину 1..24, по умолчанию 3.
`nightscout_token_env` — имя переменной из `secrets.env` с access token
Nightscout; для публичного чтения токен не нужен.
`youtube` не требует отдельной программы: звук извлекается и декодируется
самим демоном. При пустом `youtube_proxy` включается встроенный ByeDPI;
заданный внешний прокси имеет приоритет.
Таймеры и будильники сохраняются в `state_dir` и восстанавливаются после
перезапуска; будильник звонит до `dismiss`/`snooze`.

## `[modules]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `youtube` | `true` | поиск и звук YouTube в процессе |
| `web_search` | `true` | Tavily (нужен ключ) |
| `weather` | `true` | погода Open-Meteo |
| `nightscout` | `true` | глюкоза, если задан URL |
| `display` | `true` | экран и светодиоды |
| `spotify` | `true` | встроенный Spotify Connect |
| `byedpi` | `true` | встроенный обход YouTube |

Выключенный модуль не создаёт клиентов, потоков и фоновых задач.

## `[spotify]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `device_name` | `Петрович` | имя в Spotify Connect |
| `username` | `""` | логин Spotify |
| `password_env` | `PETROVICH_SPOTIFY_PASSWORD` | пароль в secrets.env |
| `access_token_env` | `PETROVICH_SPOTIFY_TOKEN` | альтернатива паролю |
| `client_id` | `""` | резервный client id, если нет токена Connect-сессии |
| `client_secret_env` | `PETROVICH_SPOTIFY_CLIENT_SECRET` | секрет резервного client id |
| `market` | `RU` | рынок поиска |
| `cache_dir` | `<state_dir>/spotify` | кэш librespot, предел 64 МБ |
| `bitrate` | `160` | 96, 160 или 320 кбит/с |
| `search_limit` | `10` | результатов на тип поиска |

Connect-устройство регистрируется сразу после старта и остаётся видимым,
даже когда музыка не играет. Пароль/токен и client secret кладутся только
в `secrets.env`; без них модуль не запускается и ничего не занимает.
Управление: play/pause/next/previous, seek и seek_to, очередь, sleep,
сохранение позиции (librespot/Spotify Connect).

## `[byedpi]`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `listen_port` | `0` | порт loopback SOCKS5; 0 — свободный |
| `strategies` | пресеты ByeByeDPI | порядок проверки |
| `refresh_s` | `300` | период перепроверки |
| `probe_timeout_ms` | `4000` | таймаут одной проверки |
| `probe_video` | `jNQXAC9IVRw` | ролик для проверки медиапотока |
| `state_path` | `<state_dir>/byedpi.json` | запомненная стратегия |

SOCKS5 слушает только `127.0.0.1`; глобальные настройки сети не меняются.
Desync применяется только к доменам YouTube/Google Video. Поддерживаются
символические имена (`split_sni`, `disorder_sni`, `oob_sni`,
`fake_ttl_sni`, `split_N`) и пресеты ByeByeDPI/ciadpi (`-d`, `-f`, `-o`, `-s`).
Стратегию можно прописать вручную первой строкой списка, включая цепочку
разрезов с флагами SNI, например:

```toml
[byedpi]
strategies = [
  "-d1 -s1+s -s3+s -s6+s -s12+s -s14+s -s20+s -s24+s -s30+s -a1",
  "split_sni",
]
```

Порядок — приоритет проверки; выбранная стратегия сохраняется в
`state/byedpi.json`.
Выбор проверяется реальным декодированием первого аудиопакета, а не только
HTTP-доступностью; при сбое стратегии перебор повторяется.

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
