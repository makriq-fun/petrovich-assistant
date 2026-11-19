# Провайдеры STT, LLM и TTS

Все три компонента заменяемы. Сетевые провайдеры говорят на
OpenAI-совместимом HTTP; локальные могут быть командами. HTTPS обязателен,
кроме loopback (`localhost`, `127.0.0.1`, `::1`) — это удобно для локального
whisper.cpp/SearXNG. Сертификаты проверяются rustls; отключения проверки нет.

## Секреты

```env
SKYNET_STT_API_KEY=...
SKYNET_LLM_API_KEY=...
SKYNET_TTS_API_KEY=...
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
api_key_env = "SKYNET_STT_API_KEY"
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
# tls_ca_file = "/data/skynet/certs/local-ca.crt"
```

Запрос: `POST /asr`, multipart `audio`, `language`, `diarize=false`,
`tag_audio_events=false`. Ответ — JSON с `text`. Модель `transcribe-1-pro`
требует кредита Fish; тариф может отличаться.

Локальный вариант:

```toml
[stt]
provider = "command"
timeout_ms = 60000
command = ["whisper-cli", "-m", "/data/skynet/models/ggml-base.bin",
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
api_key_env = "SKYNET_LLM_API_KEY"
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
api_key_env = "SKYNET_TTS_API_KEY"
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

Пример управления Spotiya:

```toml
[tools]
music_command = ["spotiya"]
# альтернатива с явным шаблоном:
# music_command = ["/data/skynet/music.sh", "{action}"]
```

Spotiya должна быть установлена и настроена отдельно; проект управляет
только командами play/pause/resume/next/previous/status.

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
фраз. Ошибка TTS прерывает ход; автомат играет звук ошибки.

## Погода

Погода — не отдельный провайдер, а инструмент:

```toml
[tools]
weather_url = "https://api.open-meteo.com/v1/forecast"
weather_latitude = 55.7558
weather_longitude = 37.6173
weather_place = "Москва"
```

Open-Meteo не требует ключа. Ответ содержит текущую температуру, ощущаемую,
влажность, ветер, осадки и дневной диапазон; коды WMO переводятся на русский.
Endpoint можно заменить совместимым. Если координаты не заданы, инструмент
не публикуется для LLM.

## Проверка

```bash
skynetd --config config.toml --check-config
```

Показывает провайдеров, наличие каждого секрета (только имя), VQE и файлы
модели. Реальные запросы проверяются только запуском; тесты используют
локальный mock-сервер и не тратят квоты.
