# VexMap

Карта мира в духе Google Maps и Google Earth внутри Minecraft 26.3 на [Horizon](https://github.com/horizon-rust/horizon).
Нативный мод для клиента и для серверов регионов. Лицензия MIT.

Состояние: Фаза 2, ядро данных и локальный API. План и решения лежат в [docs/](docs/).

## Устройство

```text
mods/vexmap/                 бандл horizon-mixins: один мод на клиент и сервер
  compatibility.json         пин Horizon и horizon-mixins
  mixins/vexmap/proto.rs     протокол, только std, он же крейт vexmap-proto
  mixins/vexmap/runtime.rs   связь с API Horizon
crates/vexmap-proto/         Cargo-обёртка над proto.rs для тестов и sidecar
crates/vexmap-core/          игроки, роли, выключатели модулей, журнал изменений (SQLite и PostgreSQL)
crates/vexmap-sidecar/       отдельный процесс: ядро данных за локальным REST API
tools/dev/stage.py           применить мод к Horizon и запустить cargo (Windows и Linux)
tools/release/sign.py        архивы релиза, SHA256SUMS, подпись minisign
```

## Команды

```sh
cargo test --workspace                                # протокол, без сборки игры
python tools/dev/stage.py check mods/vexmap           # мод внутри настоящего Horizon
python tools/dev/stage.py build mods/vexmap           # клиент с модом, профиль fast
cargo run -p vexmap-sidecar                           # локальный API на 127.0.0.1:7878, база SQLite рядом
```

`stage.py` ждёт рядом `.deps/horizon-mixins` на ревизии из `compatibility.json`.

## Проверка релиза

Релизы подписаны [minisign](https://jedisct1.github.io/minisign/). Публичный ключ лежит в [vexmap.pub](vexmap.pub):

```text
RWSFNDOgqs3Sr6jDLI06gzKF/1NtoK6CV9S14KIx65gPS+KLqHTvWn5o
```

```sh
minisign -Vm SHA256SUMS -p vexmap.pub
sha256sum -c SHA256SUMS
```
