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

Пример управления Spotiya:

```toml
[tools]
music_command = ["/data/opt/spotify-connect/spotiya", "--{action}"]
# альтернатива для своей обёртки:
# music_command = ["/data/petrovich/music.sh", "{action}"]
```

Spotiya должна быть установлена и настроена отдельно. Если `music_command`
пуст, инструмент `music` не публикуется для LLM. `play` без названия
продолжает текущий трек (`--resume`), `play` с `query` запускает поиск
(`--play QUERY`); также доступны pause/resume/next/previous/status.
Статус разбирается из JSON Spotya в короткую строку «исполнитель — трек»,
чтобы не раздувать контекст LLM.
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
youtube_proxy = "socks5h://127.0.0.1:1080"
```

Действия: `play` (ссылка, id или поисковый запрос), `pause`, `resume`,
`stop`, `status`. На время ответа и объявлений звук приглушается, затем
продолжается; явная пауза голосом не снимается автоматически.

### byedpi, а не zapret

| | byedpi (`ciadpi`) | zapret (`nfqws`) |
|---|---|---|
| процесс | один C-бинарник ~146 КБ | демон + правила iptables/nftables |
| RSS в простое | ~150 КБ | единицы-десятки МБ |
| путь пакетов | только трафик приложений, использующих SOCKS5 | весь трафик через NFQUEUE |
| цена на 4×Cortex-A53 | почти нулевая | очередь netfilter и копирование пакетов в userspace |
| риск | приложение должно уметь прокси | ошибка в правилах режет сеть целиком |

На Мини 2 выбран byedpi: он не трогает firewall и не добавляет работу
каждому пакету. Демон ходит в YouTube через `youtube_proxy`, остальной
трафик идёт напрямую.

Если на станции уже работает Hysteria-клиент (например, для Spotiya), можно
не ставить второй прокси: добавьте в его конфиг SOCKS5-вход и укажите этот
порт в `youtube_proxy`:

```json
"socks5": { "listen": "127.0.0.1:1081" }
```

Hysteria и byedpi можно держать одновременно: `youtube_proxy` выбирает один.

Установка и автозапуск (делается с ПК установщиком):

```bash
python3 installer/petrovich_install.py --transport ssh --ssh-host station byedpi
# затем в config.toml:
# [tools] youtube_proxy = "socks5h://127.0.0.1:1080"
```

Стратегия по умолчанию — `-d1 -s1+s -d1+s -s3+s -d6+s -s12+s -d14+s -s20+s -d24+s -s30+s -a1`. Если
провайдер её ломает, подберите другую (`--split 1+s --disorder 3+s`,
`--fake -1 --ttl 8`) и переустановите:

```bash
python3 installer/petrovich_install.py --ssh-host station byedpi \
  --args '--split 1+s --disorder 3+s'
```

Проверка без звука (разрешение, HLS/DASH, декодирование первого пакета):

```bash
/data/petrovich/current/petrovichd --config /data/petrovich/config.toml \
  --youtube-test 'ссылка или запрос'
# проиграть 10 секунд через ALSA и проверить звук
/data/petrovich/current/petrovichd --config /data/petrovich/config.toml \
  --youtube-play 'ссылка или запрос'
```

Если byedpi не запущен или стратегия не подходит, команда вернёт ошибку
YouTube/HTTP, а не тишину. `--youtube-test` полезен и для проверки прокси:
он печатает, используется ли `tools.youtube_proxy`.

Перед ответом «включаю» демон скачивает и декодирует первый сегмент (HLS)
или первый пакет (DASH). Если медиахост `googlevideo.com` недоступен,
инструмент вернёт ошибку, а не включит тишину. Некоторым провайдерам недостаточно byedpi: они режут сам
`googlevideo.com` по SNI. В этом случае укажите в `youtube_proxy` рабочий
SOCKS5/HTTP-прокси (подойдёт и локальный VPN-туннель).
