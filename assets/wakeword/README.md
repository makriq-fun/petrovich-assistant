# Модель активации «Скайнет»

Пара файлов из репозитория
[olegchumakov/microwakeword-models](https://github.com/olegchumakov/microwakeword-models),
каталог `skynet/`:

- `skynet_mww.json` — манифест microWakeWord v2;
- `skynet_mww.tflite` — int8-модель, streaming.

Модель обучена на слово «Скайнет» (русский). Продукт называется «Петрович»,
но встроенная модель реагирует на «Скайнет»; для слова «Петрович» нужна
отдельная модель. Автор модели — kelstar; файл
лицензии в upstream отсутствует, права принадлежат автору. Подробности — в
[THIRD_PARTY_NOTICES.md](../../THIRD_PARTY_NOTICES.md).

Для microWakeWord нужен `libtflite.so` из прошивки станции
(`/system/vendor/quasar/libs/libtflite.so`). Если библиотеки нет,
используйте openWakeWord-модель и `assets/oww.bin`.
