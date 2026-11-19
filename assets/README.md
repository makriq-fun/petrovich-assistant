# Ресурсы активации

`wakeword/petrovich.tflite` — единственная модель активации в репозитории:
openWakeWord-голова для слова «Петрович». Установщик копирует её в
`/data/petrovich/models/petrovich.tflite`.

`oww.bin` — контейнер `OWW1` с мел-фильтрами и весами эмбеддингов
openWakeWord / Google speech_embedding (Apache-2.0). Он не содержит
предобученных wake-word моделей и не содержит записей голоса. Нужен для
головы «Петрович» и любых других openWakeWord-голов.

Сторонние модели в репозиторий не входят; их можно передать установщику
через `--wakeword-model`. Формат и обучение —
[../docs/wakeword-model.md](../docs/wakeword-model.md).

Подробности лицензий — [../THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
