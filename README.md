# VexMap

Карта мира в духе Google Maps и Google Earth внутри Minecraft 26.3 на [Horizon](https://github.com/horizon-rust/horizon).
Нативный мод для клиента и для серверов регионов. Лицензия MIT.

Состояние: Фаза 1, каркас мода, протокол и конвейер сборки. План и решения лежат в [docs/](docs/).

## Устройство

```text
mods/vexmap/                 бандл horizon-mixins: один мод на клиент и сервер
  compatibility.json         пин Horizon и horizon-mixins
  mixins/vexmap/proto.rs     протокол, только std, он же крейт vexmap-proto
  mixins/vexmap/runtime.rs   связь с API Horizon
crates/vexmap-proto/         Cargo-обёртка над proto.rs для тестов и sidecar
tools/dev/stage.py           применить мод к Horizon и запустить cargo (Windows и Linux)
tools/release/sign.py        архивы релиза, SHA256SUMS, подпись minisign
```

## Команды

```sh
cargo test --workspace                                # протокол, без сборки игры
python tools/dev/stage.py check mods/vexmap           # мод внутри настоящего Horizon
python tools/dev/stage.py build mods/vexmap           # клиент с модом, профиль fast
```

`stage.py` ждёт рядом `.deps/horizon-mixins` на ревизии из `compatibility.json`.
