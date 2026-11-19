# Модель активации «Петрович»

`petrovich.tflite` — openWakeWord-голова для слова «Петрович»:

- вход `[1, 16, 96]` float32 — 16 эмбеддингов по 96;
- выход `[1, 1]` float32 — score;
- манифест не нужен, файл указывается прямо в `[wakeword] model`;
- обязателен `assets/oww.bin` — мел-фильтры и веса эмбеддингов.

Установщик копирует модель в `/data/petrovich/models/petrovich.tflite`.
Порог задаётся в `[wakeword] threshold` (на колонке — 0.5), чувствительность —
`sensitivity`.

Других моделей активации в репозитории нет. Сторонние модели можно передать
через `--wakeword-model` (`.tflite` или JSON-манифест openWakeWord/microWakeWord).

Подробности — [../../docs/wakeword-model.md](../../docs/wakeword-model.md).
