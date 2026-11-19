# Ресурсы активации

`wakeword/` — встроенная сторонняя модель microWakeWord «Скайнет»:
`skynet_mww.json` и `skynet_mww.tflite`. Установщик копирует их в
`/data/petrovich/models/`.

`oww.bin` — контейнер `OWW1` с мел-фильтрами и весами эмбеддингов
openWakeWord / Google speech_embedding (Apache-2.0). Он не содержит
предобученных wake-word моделей и не содержит записей голоса. Нужен только
для openWakeWord-модели.

Модель головы openWakeWord (`wakeword.json`) сюда не входит: она обучается
на голосе владельца и устанавливается отдельно. Формат и обучение —
[../docs/wakeword-model.md](../docs/wakeword-model.md).

Подробности лицензий — [../THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
