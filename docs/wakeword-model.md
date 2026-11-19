# Модель активации

## Что входит в релиз

В `assets/wakeword/` лежит готовая пара microWakeWord v2:

- `skynet_mww.json` — манифест (`type: micro`, порог 0.9, окно 10);
- `skynet_mww.tflite` — int8 streaming-модель.

Источник — [olegchumakov/microwakeword-models](https://github.com/olegchumakov/microwakeword-models),
каталог `skynet/`. Модель обучена на слово «Скайнет» (русский). Продукт
называется «Петрович», но встроенная модель по-прежнему реагирует на
«Скайнет»: слово активации задаётся моделью, а не именем проекта. Для
активации словом «Петрович» нужна отдельная обученная модель. Автор
встроенной модели — kelstar; файл лицензии в upstream отсутствует, права
принадлежат автору. Подробности —
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).

Установщик копирует манифест и `.tflite` в `/data/petrovich/models/` и указывает
на них `[wakeword] model`.

## Требования

- `libtflite.so` из прошивки: `/system/vendor/quasar/libs/libtflite.so`;
- фронтенд TFLM microfrontend компилируется в `petrovich-wakeword` из
  Apache-2.0 исходников в `third_party/`;
- вход — 16 кГц моно, кадр 10 мс.

Если `libtflite.so` нет, microWakeWord не загрузится. В этом случае
используйте openWakeWord-модель и `assets/oww.bin` (см. ниже) либо выключите
активацию и запускайте диалог кнопкой.

Версия `libtflite.so` должна быть не ниже `min_runtime_version` модели.
Встроенная модель собрана с `2.22.0-rc0`; на прошивке `2020.02.1` стоит
`libtflite` 2.12, поэтому модель не запускается. `petrovichd` сравнивает версии
и возвращает ошибку вместо падения; для такой прошивки используйте
openWakeWord.

## Проверка

```bash
/data/petrovich/current/petrovichctl \
  --config /data/petrovich/config.toml wake-test take.wav
```

`wake-test` принимает WAV PCM16, приводит к 16 кГц/моно и печатает score по
шагам. Для калибровки используйте записи из той же комнаты и тот же тракт
(`vqe_0`/микрофоны). Порог и `gain` подбирайте на отложенных записях, а не по
паспорту.

## Тюнинг

- `probability_cutoff` в манифесте — базовый порог; `[wakeword] threshold`
  переопределяет его;
- `sensitivity` отрицательный — чаще срабатывания, положительный — реже;
- `gain` (по умолчанию 32) компенсирует уровень `vqe_0`; слишком большой
  клиппирует;
- `refractory_ms` защищает от повторного срабатывания на хвосте слова.

Меняйте по одному параметру и проверяйте на записях, которых не было в
обучении.

## Альтернатива: openWakeWord

Проект по-прежнему поддерживает openWakeWord-совместимый формат:

- `assets/oww.bin` — контейнер `OWW1` с мел-фильтрами и весами эмбеддингов;
- `wakeword.json` — голова модели: `name`, `threshold`, `head` с весами
  `w1/b1/w2/b2/w3/b3` (16 эмбеддингов по 96, окно 1,28 с).

Детектор требует два подряд окна выше порога и затем рефрактерный период.
Такой вариант не зависит от `libtflite.so`, но модель головы нужно обучить
или импортировать отдельно:

```bash
python3 installer/petrovich_install.py --ssh-host station install \
  --wakeword-model ./wakeword.json
```

Установщик кладёт выбранный файл в `/data/petrovich/models/skynet_mww.json`
(универсальное имя) и, если в манифесте есть поле `model`, рядом — указанный
`.tflite`. Так работает и microWakeWord, и openWakeWord-модель.

Готовую TFLite-голову openWakeWord (вход `[1, 16, 96]`, выход score) можно
указать прямо в `[wakeword] model`; манифест для неё не нужен, но `assets`
обязателен, а `gain` обычно равен `1`. Такой файл установщик пока не
загружает — положите его в `/data/petrovich/models/` вручную.

## Лицензии

- Код feature extraction написан заново по открытой реализации openWakeWord
  (Apache-2.0).
- `assets/oww.bin` — мел-фильтры и веса эмбеддингов openWakeWord/Google
  speech_embedding.
- Предобученные wake-word модели openWakeWord (CC BY-NC-SA 4.0) **не**
  включены.
- Модель «Скайнет» распространяется отдельно от AGPL-кода; условия уточняйте
  у автора.
