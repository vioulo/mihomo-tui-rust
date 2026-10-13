# mihomo-tui

A terminal UI for the [mihomo](https://github.com/MetaCubeX/mihomo) (Clash.Meta)
external controller. It talks to the same REST API the web dashboard uses, so it
works against any mihomo instance that exposes `external-controller` — no extra
agent, no config changes beyond the controller block.

## Features

Four tabs, switchable with `1`–`4` or `tab`:

| Tab | What it shows |
| --- | --- |
| **Overview** | Version, current routing mode, inbound ports, log level / LAN / IPv6 / TUN flags, live up/down rates with sparklines, cumulative transfer totals |
| **Proxies** | Every selector-ish group and its members, each with measured delay and a latency bar; select a member, test one member or the whole group |
| **Connections** | Live connection table (destination, type, proxy chain, process, up/down, matched rule) with a detail pane; close one or all |
| **Logs** | The live `/logs` stream, colour-coded by level |

The UI is a single-threaded render loop; all network I/O happens on background
tasks behind a pair of channels, so nothing blocks on a slow controller. The
`/traffic` and `/logs` streams reconnect on their own when mihomo drops them
(which it does on every config reload).

## Requirements

- A running mihomo with the external controller enabled:

  ```yaml
  external-controller: 127.0.0.1:9090
  secret: ""            # optional; pass it with --secret if set
  ```

- Rust 1.85+ (edition 2024) to build from source.

## Build

```sh
cargo build --release
# binary at target/release/mihomo-tui
```

## Usage

```
mihomo-tui [OPTIONS]

OPTIONS:
    --endpoint <URL>     Controller address          [default: http://127.0.0.1:9090]
    --secret <SECRET>    Bearer token, if the controller requires one
    --delay-url <URL>    URL used for latency tests
                                                    [default: http://www.gstatic.com/generate_204]
    --timeout <MS>       Per-test timeout in ms      [default: 5000]
    --refresh <MS>       State refresh interval in ms [default: 2000]
    --log-level <LEVEL>  Log stream level: debug|info|warning|error [default: info]
    -h, --help           Print this help
```

Both `--flag value` and `--flag=value` are accepted. An endpoint without a
scheme (`127.0.0.1:9090`) is completed to `http://`.

```sh
mihomo-tui --secret hunter2
mihomo-tui --endpoint 192.168.1.10:9090 --refresh 1000 --log-level warning
```

## Keys

| Key | Action |
| --- | --- |
| `1`–`4` / `tab` / `shift-tab` | Switch tab |
| `q` / `ctrl-c` | Quit |
| `r` | Refresh now |
| `m` / `M` | Cycle routing mode forward / back *(Overview)* |
| `j` / `k` | Move down / up in a list |
| `h` / `l` | Move between members *(Proxies)* |
| `enter` / `space` | Select the highlighted member *(Proxies)* |
| `t` / `T` | Test the highlighted member / the whole group *(Proxies)* |
| `x` / `X` | Close the highlighted connection / all connections *(Connections)* |
| `c` | Clear the log buffer *(Logs)* |

Latency tests use the group's own `url` when it defines one, falling back to
`--delay-url`. The terminal needs to be at least 48×12; below that the app shows
a hint instead of drawing a mangled layout.

## Development

```sh
cargo test              # unit + render-snapshot tests
cargo clippy --all-targets
cargo fmt
```

Rendering is tested against ratatui's `TestBackend`, so the tests assert on the
text a user would actually see rather than on internal state alone. The API
types are tested against captured mihomo payloads — including `"connections":
null`, which mihomo sends whenever nothing is open.
