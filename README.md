# mihomo-tui

mihomo（Clash.Meta）外部控制器的终端界面。

**简体中文** | [English](README.en.md)

它对接的是 mihomo Web 面板用的那套 REST API，所以只要能开 `external-controller`，就能直接用——不需要额外装 agent，也不用改配置以外的东西。

## 功能

五个标签页，用 `1`–`5` 或 `tab` 切换：

| 标签页 | 内容 |
| --- | --- |
| **Overview** | 版本、当前路由模式、入站端口、日志级别 / LAN / IPv6 / TUN 开关，实时上下行速率与波形图，累计流量 |
| **Proxies** | 所有分组及其成员，带实测延迟和延迟条；选中节点、单节点测速、整组测速 |
| **Connections** | 实时连接表（目标、类型、代理链、进程、上下行、命中规则）加详情面板；关闭单条或全部连接 |
| **Logs** | 实时 `/logs` 流，按级别着色 |
| **Subscriptions** | 导入/更新/删除订阅，按块 diff 合并 base 配置并热加载，策略记忆与并发测速 |

界面是单线程渲染循环，所有网络 I/O 都在后台任务里、通过一对 channel 通信，所以控制器慢也不会卡住界面。`/traffic` 和 `/logs` 两条流在断开后会自动重连（mihomo 每次重载配置都会断它们）。

## 环境要求

- 一个开好外部控制器的 mihomo：

  ```yaml
  external-controller: 127.0.0.1:9090
  secret: ""            # 可选；设了就用 --secret 传进来
  ```

- 从源码构建需要 Rust 1.85+（edition 2024）。

## 构建

```sh
cargo build --release
# 产物在 target/release/mihomo-tui
```

## 怎么用起来

### 第一步：确认控制器开着

TUI 需要一个**监听 TCP 端口**的外部控制器。先确认：

```sh
curl -s http://127.0.0.1:9090/version
# 期望：{"meta":true,"version":"1.19.32"}
```

如果连不上，就在 mihomo 配置里加上 `external-controller: 127.0.0.1:9090` 并重启 mihomo。设了 `secret` 的话，裸 curl 会返回 401，这是对的——启动 TUI 时带上 `--secret` 即可。

### 第二步：启动

```sh
# 默认连 127.0.0.1:9090
./target/release/mihomo-tui

# 带 secret
./target/release/mihomo-tui --secret 你的密钥

# 连远程控制器
./target/release/mihomo-tui --endpoint 192.168.1.10:9090 --secret 你的密钥
```

进去之后按 `4` 看日志、`2` 选节点、`t` 测速。`q` 退出。

### 如果你用的是 Clash Verge

Verge 默认**不开 TCP 控制器端口**，它只用 unix socket 和内核通信：

```
/run/clash-verge-service/users/1000/verge-mihomo.sock
```

这种状态下本 TUI 连不上，因为当前版本只支持 TCP 端点。两条路：

1. **打开 TCP 端口**（推荐，改动最小）：在 Verge 设置里把外部控制端口开出来（界面里通常叫「外部控制」/「External Controller」，比如设成 `127.0.0.1:9090`），然后正常启动 TUI。
2. **等 unix socket 支持**：见下方「后续优化」第 1 条。

### 命令行参数

```
mihomo-tui [OPTIONS]

OPTIONS:
    --endpoint <URL>     控制器地址            [默认: http://127.0.0.1:9090]
    --secret <SECRET>    Bearer token，控制器要求时填
    --delay-url <URL>    测速用的目标 URL
                                              [默认: http://www.gstatic.com/generate_204]
    --timeout <MS>       单项测速超时（毫秒）  [默认: 5000]
    --refresh <MS>       状态刷新间隔（毫秒）  [默认: 2000]
    --log-level <LEVEL>  日志流级别: debug|info|warning|error [默认: info]
    --config-dir <DIR>   状态目录（profiles、base/runtime 配置、TUI 设置）[默认: ~/.config/mihomo-tui]
    --concurrency <N>    并发测速数 [默认: 8]
    -h, --help           打印帮助
```

`--flag value` 和 `--flag=value` 都收。端点不带 scheme（`127.0.0.1:9090`）会自动补成 `http://`。

```sh
mihomo-tui --secret hunter2
mihomo-tui --endpoint 192.168.1.10:9090 --refresh 1000 --log-level warning
```

## 快捷键

| 按键 | 作用 |
| --- | --- |
| `1`–`4` / `tab` / `shift-tab` | 切换标签页 |
| `q` / `ctrl-c` | 退出 |
| `r` | 立即刷新 |
| `m` / `M` | 向后 / 向前切换路由模式 *(Overview)* |
| `j` / `k` | 在列表里下移 / 上移 |
| `h` / `l` | 在成员间移动 *(Proxies)* |
| `enter` / `空格` | 选中高亮的成员 *(Proxies)* |
| `t` / `T` / `A` | 测试高亮成员 / 整组 / 当前订阅全部节点（去重）*(Proxies)* |
| `s` | 按延迟排序成员 *(Proxies)* |
| `[` / `]` | 循环切换上一个 / 下一个订阅 *(全局)* |
| `v` | 重新应用当前订阅（吸收外部编辑）*(全局)* |
| `a` / `u` / `d` / `enter` | 添加 / 更新 / 删除 / 切换订阅 *(Subscriptions)* |
| 鼠标 | 点击标签页、代理成员、订阅行（可在 config.json 关闭） |
| `x` / `X` | 关闭高亮连接 / 全部连接 *(Connections)* |
| `c` | 清空日志缓冲 *(Logs)* |

测速优先用分组自己配置的 `url`，没有才回落到 `--delay-url`。终端至少要有 48×12，小于这个尺寸会显示提示而不是画出错乱的布局。

## 订阅与配置

TUI 维护自己的 `base.yaml`，导入的订阅快照缓存在 `~/.config/mihomo-tui/subscriptions/<id>.yaml`。切换订阅时按配置块（proxies / proxy-groups / rules / dns / …）diff，冲突逐块确认；每个订阅记住上次的 base/订阅选择作为下次预选，无差异时直接切换。确认后生成 `runtime.yaml`（原子写入、0600）并热加载到控制器，失败自动回滚。

外部修改 `base.yaml` 或订阅快照不会被静默应用：TUI 进入待确认状态，按 `v` 查看 diff 后再重载。`runtime.yaml` 含 `external-controller` 与 `secret`，可以把系统服务指向它持久化；注意它落盘了一份凭据，权限 0600。

监听是 1 秒轮询 `(mtime, size)`，不引入额外依赖。

### 状态目录

```
~/.config/mihomo-tui/
  config.json        # TUI 设置（并发数、鼠标开关）
  base.yaml          # 你维护的基础配置
  runtime.yaml       # 生成并交给 mihomo 加载的配置（0600）
  profiles.json      # 订阅索引与策略记忆（0600）
  subscriptions/     # 每条订阅的快照（0600）
```

## 开发
## 开发

```sh
cargo test               # 单元测试 + 渲染快照测试
cargo clippy --all-targets
cargo fmt
```

渲染测试跑在 ratatui 的 `TestBackend` 上，断言的是用户真正会看到的文字，而不只是内部状态。API 类型用真实抓下来的 mihomo 响应做测试——包括 `"connections": null`，这是 mihomo 在没有连接时发的。

## 后续优化

按优先级排，前三条是我实际跑测后确认存在的缺陷。

1. **支持 unix socket 端点** — 目前只能连 TCP，导致 Clash Verge 这类只用 unix socket 的部署连不上。reqwest 已经有 `ClientBuilder::unix_socket()`（`#[cfg(unix)]`，不需要额外 feature），加一个 `--unix-socket <PATH>` 参数即可，改动集中在 `api.rs` 的客户端构造。
2. **连接列表没有上限，也没有虚拟化** — `ui.rs` 每次重绘都会为全部连接构建 `Row`。连接数上千时（P2P、浏览器开一堆 tab）会明显掉帧。修法是只构建可见的那几十行，或加上限加过滤。
3. ~~整组测速的进行中标志有竞态~~ — 已迁入并发池，按代计数 — 按 `T` 会把 `testing` 置真，但单节点结果回来时也会无条件把它清掉。先按 `T` 再按 `t`，单个结果一到，「testing…」就提前消失了。需要把单节点和整组两个状态分开，或者用计数器。
4. **搜索 / 过滤** — 分组和连接多了以后找东西很费劲，想要一个 `/` 就地过滤。
5. **日志页的滚动与暂停** — 现在只能看尾部，没法暂停滚回去看。空白键暂停 + `j/k` 滚动就够用。
6. ~~配置持久化~~ — `config.json` 已落地（并发数、鼠标），其余仍待补充
7. **CI** — 加个 GitHub Actions 跑 `fmt` / `clippy` / `test`，防止回归。

一些更小的点：延迟条的 1000ms 满刻度是硬编码的；流量波形固定 120 个采样，不随终端宽度自适应；颜色主题也是写死的。

---

本地实测环境：mihomo 1.19.32，Arch Linux。已针对真实控制器验证：连接与认证、四个标签页、单节点与整组测速、切换节点、关闭单条/全部连接、模式切换、实时流量与日志流、断连降级。
