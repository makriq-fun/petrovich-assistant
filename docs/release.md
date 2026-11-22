# Выпуск

Релизы собирает CI. Локальная сборка нужна только для отладки: архивы в
`dist/` не коммитятся и в выпуск не попадают.

## Что делает CI

Workflow [.github/workflows/ci.yml](../.github/workflows/ci.yml) запускается
на push в `main`, на pull request, на тег `v*` и вручную
(`workflow_dispatch`).

| Job | Когда | Что проверяет |
|---|---|---|
| `test` | всегда | `cargo fmt --check`, clippy с `-D warnings`, `cargo test --workspace`, тесты установщика, синтаксис shell-скриптов |
| `package` (`armv7`) | после `test` | сборка через `cross` под `armv7-unknown-linux-gnueabihf`, запуск обоих бинарников под qemu, упаковка, проверка `SHA256SUMS` и состава архива |
| `package` (`host`) | после `test` | сборка под `x86_64-unknown-linux-gnu`, запуск `--version`, упаковка и те же проверки |
| `release` | только теги `v*` | сверка тега с `Cargo.toml`, проверка скачанных архивов по контрольным суммам, публикация GitHub Release с автоматическими заметками |

Артефакты выпуска:

- `petrovich-assistant-<версия>-armv7.tar.gz` — для колонки;
- `petrovich-assistant-<версия>-host.tar.gz` — для проверки на ПК;
- `SHA256SUMS` — обе суммы в одном файле.

Архивы собираются воспроизводимо (`tar --sort=name`, фиксированные владелец
и mtime), поэтому повторная сборка из того же коммита должна совпасть по
SHA256 и её можно сверить с опубликованной.

## Порядок выпуска

Версия живёт в `Cargo.toml` (`[workspace.package] version`); все crates
наследуют её, установщик читает её же. Отдельного файла версии нет.

1. Поднять версию в `Cargo.toml` и обновить lock:

   ```bash
   cargo update -w
   ```

2. Прогнать те же проверки, что и CI:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo test --workspace --locked
   python3 -m unittest discover -s installer/tests -v
   ```

3. Закоммитить изменения и запушить `main`.

4. Поставить аннотированный тег и запушить его:

   ```bash
   git tag -a v0.2.9 -m "Петрович 0.2.9"
   git push origin main
   git push origin v0.2.9
   ```

5. Дождаться `release` job. Если тег не совпадает с версией в `Cargo.toml`,
   job падает до публикации — это защита от выпуска чужой версии.

Повторный запуск без нового тега: Actions → `ci` → Run workflow, выбрать
тег в поле «Use workflow from». Пересоздавать тег не нужно, пока выпуск не
опубликован; после публикации исправления идут новой версией.

## Локальная сборка

Только для отладки, выпуск она не заменяет:

```bash
scripts/build.sh            # ARMv7, как в CI
TARGET=host scripts/build.sh
```
