# Провайдеры STT, LLM и TTS

Все три компонента заменяемы. Сетевые провайдеры говорят на
OpenAI-совместимом HTTP; локальные могут быть командами. HTTPS обязателен,
кроме loopback (`localhost`, `127.0.0.1`, `::1`) — это удобно для локального
whisper.cpp/SearXNG. Сертификаты проверяются rustls; отключения проверки нет.

## Секреты

```env
PETROVICH_STT_API_KEY=...
PETROVICH_LLM_API_KEY=...
PETROVICH_TTS_API_KEY=...
```

Файл `secrets.env` должен иметь права `0600` (или `0400`). Значения не
попадают в `Debug`, журналы и сообщения об ошибках; длинные токены в телах
ошибок редактируются. Ссылка на ключ — `api_key_env` в каждой секции.
Пустой ключ допустим: локальный сервер может не требовать авторизации.

## STT

```toml
[stt]
provider = "openai"
base_url = "https://api.openai.com/v1"
model = "whisper-1"
api_key_env = "PETROVICH_STT_API_KEY"
language = "ru"
timeout_ms = 30000
retries = 2
```

Запрос: `POST {base_url}/audio/transcriptions`, multipart:
`file=audio.wav` (PCM16, 16 кГц, моно), `model`, `language`,
`response_format=json`. Ожидается `{"text": "..."}`.

Fish Audio ASR (`provider = "fish"`):

```toml
[stt]
provider = "fish"
base_url = "https://api.fish.audio/v1"
model = "transcribe-1-pro"   # HTTP-заголовок model
api_key_env = "FISH_API_KEY"
language = "ru"
timeout_ms = 120000
# для self-hosted HTTPS с собственным CA:
# tls_ca_file = "/data/petrovich/certs/local-ca.crt"
```

Запрос: `POST /asr`, multipart `audio`, `language`, `diarize=false`,
`tag_audio_events=false`. Ответ — JSON с `text`. Модель `transcribe-1-pro`
требует кредита Fish; тариф может отличаться.

Локальный вариант:

```toml
[stt]
provider = "command"
timeout_ms = 60000
command = ["whisper-cli", "-m", "/data/petrovich/models/ggml-base.bin",
           "-f", "{file}", "-l", "ru", "-nt"]
```

`{file}` заменяется временным WAV; без него WAV подаётся на stdin. Вывод —
текст или JSON с полем `text`. Команда запускается без shell, с
`kill_on_drop` и тайм-аутом.

## LLM

```toml
[llm]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
# Необязательно: резервные модели того же endpoint.
fallback_models = ["gpt-4o"]
api_key_env = "PETROVICH_LLM_API_KEY"
temperature = 0.4
max_tokens = 800
stream = true
tools = true
timeout_ms = 60000
retries = 2
system_prompt = "..."
```

`POST {base_url}/chat/completions`. При `stream=true` разбирается SSE:
`choices[0].delta.content` и `delta.tool_calls` (id/name/arguments
накапливаются по `index`). `[DONE]`, `finish_reason`, `usage`
поддерживаются. При `stream=false` — обычный JSON.

Если модель вернула 429, 404 `model_not_found` или 400 `tool_use_failed`,
шлюз сразу пробует следующую из `fallback_models`; 5xx и сетевые ошибки
повторяются на той же модели `retries` раз. 401 и прочие ошибки запроса
резерв не включают — это ошибка конфигурации, а не модели.

Инструменты передаются как OpenAI `tools`; при непустом списке добавляется
`tool_choice=auto`. Демон исполняет вызовы, добавляет `tool`-сообщения и
повторяет запрос до `dialog.max_tool_rounds`.

Повторы: 429, 5xx и сетевые ошибки; экспоненциальная пауза. Если поток уже
начался, повтор не делается, чтобы не произнести ответ дважды.

## TTS

```toml
[tts]
provider = "openai"
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini-tts"
voice = "alloy"
api_key_env = "PETROVICH_TTS_API_KEY"
format = "pcm"
sample_rate = 24000
timeout_ms = 30000
max_bytes = 16777216
```

`POST {base_url}/audio/speech` с `response_format` `pcm` или `wav`.
`pcm` — поток сырых S16LE моно с частотой `sample_rate`; байты сразу уходят в
очередь воспроизведения. `wav` буферизуется, заголовок проверяется, тело
играется как PCM; потоковым этот режим не является.

Fish Audio TTS (`provider = "fish"`):

```toml
[tts]
provider = "fish"
base_url = "https://api.fish.audio/v1"
model = "s2.1-pro-free"
voice = "<reference_id голоса>"
api_key_env = "FISH_API_KEY"
format = "pcm"
sample_rate = 24000
timeout_ms = 60000
```

Запрос: `POST /tts`, заголовок `model`, JSON `text`, `reference_id`,
`format=pcm`, `sample_rate`. Ответ — сырой S16LE моно, играется потоком.
`s2.1-pro-free` доступен на бесплатном тарифе; платные модели требуют кредит.

Локальный TTS:

## Spotify

Стандартный путь — встроенный Spotify Connect на librespot в `petrovichd`:

```toml
[modules]
spotify = true

[spotify]
device_name = "Петрович"
username = "user@example.com"
# Для входа достаточно пароля или access token; после первого входа
# librespot хранит credentials.json в cache_dir.

# secrets.env, права 0600:
# PETROVICH_SPOTIFY_PASSWORD=...
# PETROVICH_SPOTIFY_TOKEN=...
```

Устройство видно в Spotify Connect сразу после старта, в том числе на паузе.
Поиск идёт через Spotify Web API токеном Connect-сессии, звук — через librespot
в тот же ALSA `default`, без отдельного приложения. Поддерживаются треки,
альбомы, плейлисты, подкасты и выпуски, seek, очередь, таймер сна и
сохранение позиции. Поиск использует токен уже авторизованной Connect-сессии.
Если сессия импортирована старым access token, keymaster недоступен: тогда
работает резервный поиск Tavily по ссылкам `open.spotify.com` (нужен ключ
`[tools] search_api_key_env`) или client credentials приложения Spotify.
Без этого поиск честно сообщает об ошибке, а не подменяет Spotify на YouTube.

Устаревший внешний backend остаётся только как совместимость; при включённом
Spotify он не используется:

```toml
[tools]
music_command = ["/data/opt/spotify-connect/spotiya", "--{action}"]
# альтернатива для своей обёртки:
# music_command = ["/data/petrovich/music.sh", "{action}"]
```

Если Spotify выключен и `music_command` пуст, инструмент `music` не
публикуется для LLM. `play` без названия продолжает текущий трек, `play` с
`query` запускает поиск; также доступны pause/resume/next/previous/status,
seek, seek_to, queue и sleep.
Перед `load`/`play` демон активирует устройство Connect: без этого librespot
молча игнорирует команды после перезапуска, когда активным числится телефон.
Музыка приглушается на время слушания и речи, а не только во время ответа:
звук идёт мимо AEC-референса и иначе попадает в STT. Запуск трека снимает
приглушение сразу, чтобы «включаю» не произносилось в тишину.
Те же действия без слова активации делает штатный command spotter из
прошивки (`[commands]`, раздел `[commands]` в configuration.md): «дальше»,
«назад», «играй», «продолжи» вызывают `next/previous/resume`, «громче/тише» —
громкость, «пауза/стоп/хватит» — глобальную остановку. Это не LLM-инструмент
и не требует сети.

```toml
[tts]
provider = "command"
format = "pcm"
sample_rate = 24000
timeout_ms = 30000
command = ["rhvoice-client", "-v", "anna", "-r", "24000"]
```

Текст подаётся на stdin, PCM/WAV — на stdout. Для WAV заголовок
проверяется, вывод ограничен `max_bytes`.

## Поток и предложения

LLM отдаёт текст дельтами; `SentenceChunker` режет его на фразы по
`.!?…` с учётом сокращений. Каждая фраза синтезируется и проигрывается
последовательно. Это даёт раннее начало речи, но не параллелит TTS разных
фраз ответа. Служебные фразы таймеров и будильников кэшируются, а следующая
фраза такого сообщения синтезируется во время проигрывания текущей.
Ошибка TTS прерывает ход; автомат играет звук ошибки.

## Погода

Погода — не отдельный провайдер, а инструмент:

```toml
[tools]
weather_url = "https://api.open-meteo.com/v1/forecast"
weather_geocoding_url = "https://geocoding-api.open-meteo.com/v1/search"
weather_latitude = 55.7558
weather_longitude = 37.6173
weather_place = "Москва"
```

Open-Meteo не требует ключа. Ответ содержит текущую температуру, ощущаемую,
влажность, ветер, осадки и дневной диапазон; коды WMO переводятся на русский.
Endpoint можно заменить совместимым. Координаты задают место по умолчанию;
`location` в вызове инструмента ищется через геокодинг. Без координат
инструмент работает только с явно названным городом. Если `weather_url`
пуст, инструмент не публикуется для LLM.

## Проверка

```bash
petrovichd --config config.toml --check-config
```

Показывает провайдеров, наличие каждого секрета (только имя), VQE и файлы
модели. Реальные запросы проверяются только запуском; тесты используют
локальный mock-сервер и не тратят квоты.

## YouTube только звуком

Инструмент `youtube` играет аудиодорожку целиком, без картинки и без
`yt-dlp`/`mpv`: демон сам получает HLS-манифест через InnerTube и декодирует
AAC (Symphonia). Если HLS у ролика нет (так бывает у подкастов), берётся
DASH-аудио MP4: демон читает его range-запросами, не загружая десятки
мегабайт в память. Это важно для Станции: на `/data` свободно около 25 МБ, а
статический `yt-dlp` занимает 40 МБ и требует Python.

Пример:

```toml
[tools]
enabled = ["time", "timer", "alarm", "volume", "music", "youtube", "emotion",
           "web_fetch", "weather", "reset_dialog", "stop"]
# Пусто — встроенный ByeDPI. Заданный внешний прокси имеет приоритет.
youtube_proxy = ""
```

Действия: `play` (ссылка, id или поисковый запрос), `pause`, `resume`,
`stop`, `status`. На время ответа и объявлений звук приглушается, затем
продолжается; явная пауза голосом не снимается автоматически.

### ByeDPI в процессе

ByeDPI-подобный обход встроен в `petrovichd`; отдельный `ciadpi`/zapret не
запускается и iptables/nftables не меняются. Локальный SOCKS5 слушает только
`127.0.0.1` на свободном порту, а `petrovichd` направляет в него только
запросы YouTube. Остальной трафик идёт напрямую.

```toml
[modules]
byedpi = true

[byedpi]
listen_port = 0          # 0 — свободный
refresh_s = 300
probe_timeout_ms = 4000
probe_video = "jNQXAC9IVRw"
```

Стратегии задаются пресетами ByeByeDPI (`-d`, `-f`, `-o`, `-s`) или
символическими именами. При старте демон проверяет их реальным декодированием
первого аудиопакета выбранного ролика, запоминает рабочую в
`state/byedpi.json` и периодически перепроверяет, когда YouTube не играет.
При сбое перебирает заново, не меняя системные настройки сети.

Если `tools.youtube_proxy` задан явно, он имеет приоритет: встроенный
ByeDPI не запускается. Это оставляет возможность использовать внешний
SOCKS5/HTTP-прокси или Hysteria.

Проверка без звука (разрешение, HLS/DASH, декодирование первого пакета):

```bash
/data/petrovich/current/petrovichd --config /data/petrovich/config.toml \
  --youtube-test 'ссылка или запрос'
# проиграть 10 секунд через ALSA и проверить звук
/data/petrovich/current/petrovichd --config /data/petrovich/config.toml \
  --youtube-play 'ссылка или запрос'
```

Если встроенный ByeDPI не подобрал стратегию или заданный внешний прокси не
работает, команда вернёт ошибку YouTube/HTTP, а не тишину. `--youtube-test`
печатает, какой прокси используется.

Перед ответом «включаю» демон скачивает и декодирует первый сегмент (HLS)
или первый пакет (DASH). Если медиахост `googlevideo.com` недоступен,
инструмент вернёт ошибку, а не включит тишину. Если провайдер режет сам
`googlevideo.com` по SNI и встроенные стратегии не помогают, укажите в
`youtube_proxy` рабочий SOCKS5/HTTP-прокси (подойдёт локальный VPN-туннель).
