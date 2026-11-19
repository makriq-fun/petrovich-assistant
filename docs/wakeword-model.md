# Модель активации «Петрович»

## Что входит в релиз

В `assets/wakeword/` лежит единственная модель проекта:

- `petrovich.tflite` — openWakeWord-голова для слова «Петрович»:
  вход `[1, 16, 96]` float32, выход `[1, 1]` float32 (score);
  манифест не нужен.

Для работы головы обязателен `assets/oww.bin` — контейнер `OWW1` с
мел-фильтрами и весами эмбеддингов openWakeWord / Google speech_embedding.
Он не содержит модели слова и записей голоса.

Установщик копирует модель в `/data/petrovich/models/petrovich.tflite` и
прописывает её в `[wakeword] model`. Других моделей активации в репозитории
нет.

## Требования

- TFLite C API: `/system/vendor/quasar/libs/libtflite.so` из прошивки либо
  `libtensorflowlite_c.so` рядом с `petrovichd`;
- вход — 16 кГц моно, кадр 10 мс;
- для «Петрович» на Станции Мини 2 рабочие значения: `threshold = 0.5`,
  `gain = 1`.

Если библиотеки TFLite нет, голова не загрузится; активацию можно выключить
и начинать диалог кнопкой.

## Проверка

```bash
/data/petrovich/current/petrovichctl \
  --config /data/petrovich/config.toml wake-test take.wav
```

`wake-test` принимает WAV PCM16, приводит к 16 кГц/моно и печатает score по
шагам. Для калибровки используйте записи из той же комнаты и тот же тракт
(`vqe_0`/микрофоны).

Проверка конфигурации без запуска аудио:

```bash
/data/petrovich/current/petrovichd \
  --config /data/petrovich/config.toml --check-config
```

## Тюнинг

- `[wakeword] threshold` — порог срабатывания; переопределяет значение из
  модели (у «Петрович» манифеста нет, поэтому задаётся только здесь);
- `sensitivity` — смещение порога: отрицательное чаще, положительное реже;
- `gain` — усиление PCM до детектора; слишком большой клиппирует;
- `refractory_ms` — пауза между срабатываниями.

Меняйте по одному параметру и проверяйте на записях, которых не было в
обучении.

## Своя модель

Установщик принимает `.tflite`-голову openWakeWord или JSON-манифест
openWakeWord/microWakeWord:

```bash
python3 installer/petrovich_install.py --ssh-host station install \
  --config config.toml --secrets secrets.env \
  --wakeword-model ./my_wakeword.tflite
```

Файл попадает в `/data/petrovich/models/`, а `[wakeword] model` в конфиге
переписывается на него. Для JSON-манифеста рядом загружается указанный в нём
`.tflite`. Сторонние модели в репозиторий не входят.

## Лицензии

- `assets/wakeword/petrovich.tflite` добавлен владельцем проекта; права на
  веса принадлежат автору модели.
- `assets/oww.bin` — мел-фильтры и веса эмбеддингов openWakeWord/Google
  speech_embedding (Apache-2.0).
- Сторонние предобученные wake-word модели (CC BY-NC-SA 4.0) **не** включены.
