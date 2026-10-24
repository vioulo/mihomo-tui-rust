# 订阅管理与配置热加载 实现计划

> **面向 AI 代理的工作者：** 必需子技能：使用 subagent-driven-development（推荐）或 executing-plans 逐任务实现此计划。步骤使用复选框（`- [ ]`）语法来跟踪进度。

**目标：** 让 mihomo-tui 能导入/切换订阅、按块 diff 合并 base 配置、经控制器热加载，并支持并发测速，从而替代 GUI 客户端。

**架构：** 保持现有"UI 单线程循环 + 后台任务 + 单对 channel"不变。新增三个后台任务：profile worker（合并/生成/重载/回滚，唯一写盘者）、外部文件轮询、测速池。新模块 `paths.rs`（目录与 TUI 自身配置）、`store.rs`（订阅持久化）、`subscription.rs`（嗅探/解析）、`merge.rs`（块 diff/合并/校验）、`runtime.rs`（运行配置渲染与原子写）。

**技术栈：** Rust edition 2024、ratatui/crossterm/tokio/reqwest（现有）；新增运行依赖 `serde_yaml_ng`、`base64`、`url`；新增 dev 依赖 `tempfile`、`wiremock`。外部文件监听用 1s 轮询比对 (mtime, size)，不引入 notify。

**规格：** `docs/superpowers/specs/2026-10-24-subscription-management-design.md`

## 全局约束

- Rust edition 2024；`cargo clippy --all-targets -- -D warnings` 与 `cargo fmt --check` 必须干净。
- 现有 56 个测试保持通过；仅 `T` 迁入并发池这一设计内行为变更允许改写对应断言。
- 测试禁止真实网络与本机 mihomo：控制器一律 wiremock，文件一律 tempfile 临时目录。
- `profiles.json`、`runtime.yaml`、`subscriptions/*` 落盘权限 0600（unix）。
- 并发测速默认 8；`--concurrency` 覆盖 `config.json`。
- 面向 mihomo 1.19.x 控制器；全程不管理内核进程、不改系统代理。
- 节点名/分组名经过 URL 路径时必须 percent-encode（沿用 `Client::url` 现有实现）。

## 审查重点（Review Focus）

1. 订阅 URL 含 token，任何落盘文件权限写错就是泄漏 → 期待 0600。测试钉在任务 2/8。
2. 畸形/恶意订阅输入（非 base64、二进制、超长行、空文件）→ 必须得到结构化错误或跳过报告，绝不 panic、不无限占内存。测试钉在任务 3/4。
3. 重载失败且回滚也失败 → UI 必须进入 offline 并显示原因，不得假成功。测试钉在任务 11。
4. base.yaml 被编辑到一半（非法 YAML）→ 待确认流程必须提示错误且不得让当前运行配置失效。测试钉在任务 12。
5. 节点名为空、重复、含 emoji/空格/斜杠 → 解析、去重、URL、分组修补都不得破坏。测试钉在任务 4/7。

---

### 任务 1：依赖与配置基座（`--config-dir`、`--concurrency`、paths.rs）

**文件：**
- 修改：`Cargo.toml`（运行依赖 serde_yaml_ng/base64/url；dev 依赖 tempfile/wiremock）
- 创建：`src/paths.rs`
- 修改：`src/main.rs`（声明模块、`parse_args` 增加两个 flag、`Options` 增加字段）
- 测试：`src/paths.rs` 内 `mod tests`、`src/main.rs` 既有参数测试扩展

- [ ] **步骤 1：编写失败的测试**

```rust
// src/paths.rs tests
#[test] fn config_dir_flag_wins_over_env() {
    // Paths::resolve(Some("/tmp/x")) 的 root == /tmp/x，无视 XDG/HOME
}
#[test] fn default_location_is_xdg_or_home() {
    // 设 XDG_CONFIG_HOME=/t1 → root=/t1/mihomo-tui；清空 XDG、HOME=/t2 → root=/t2/.config/mihomo-tui
}
#[test] fn ensure_creates_directories() { /* ensure() 后 subscriptions/ 存在 */ }
#[test] fn tui_config_defaults_and_roundtrip() {
    // load 缺失文件 → TuiConfig{concurrency:8, mouse:true}；save 后 load 相等
}
// main.rs tests 追加：
#[test] fn parses_config_dir_and_concurrency() {
    // parse(&["--config-dir","/x","--concurrency","16"]) → 对应字段；--concurrency abc 报错
}
```

- [ ] **步骤 2：运行 `cargo test` 确认失败**（模块不存在）

- [ ] **步骤 3：实现**

```rust
pub struct Paths { pub root: PathBuf }
impl Paths {
    pub fn resolve(config_dir: Option<&str>) -> Result<Self>;
    pub fn base_yaml(&self) -> PathBuf;      // base.yaml
    pub fn runtime_yaml(&self) -> PathBuf;   // runtime.yaml
    pub fn profiles_json(&self) -> PathBuf;  // profiles.json
    pub fn subscriptions_dir(&self) -> PathBuf;
    pub fn config_json(&self) -> PathBuf;
    pub fn ensure(&self) -> Result<()>;
}
#[derive(Clone, Debug)] pub struct TuiConfig { pub concurrency: usize, pub mouse: bool }
impl TuiConfig {
    pub fn load(p: &Paths) -> Self;              // 缺失/损坏 → 默认 {8, true}
    pub fn save(&self, p: &Paths) -> Result<()>; // json，普通权限
}
```
`parse_args`：`--config-dir <DIR>`、`--concurrency <N>`（N≥1，非法值报错），`Options` 加 `config_dir: Option<String>`、`concurrency: Option<usize>`。

- [ ] **步骤 4：`cargo test` 全绿**
- [ ] **步骤 5：Commit** `feat: config directory layout and tui settings`

### 任务 2：store.rs — 订阅持久化（profiles.json + 快照，0600）

**文件：**
- 创建：`src/store.rs`
- 修改：`src/main.rs`（`mod store;`）
- 测试：`src/store.rs` 内 tests（tempdir）

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn add_assigns_sequential_ids_and_persists_0600() {
    // add_profile(s1) → add_profile(s2)；profiles.json 权限 0o600；重载后字段一致
}
#[test] fn snapshot_roundtrip_and_missing_errors() { /* write/read snapshot；读取不存在 id → Err */ }
#[test] fn delete_and_policy_update() { /* delete 后列表为空；update_policy 写入 {"proxies":"subscription"} */ }
#[test] fn corrupted_profiles_json_is_an_error_not_a_panic() { /* 写入 "not json" → load_profiles 返回 Err */ }
```

- [ ] **步骤 2：`cargo test` 失败**
- [ ] **步骤 3：实现**

```rust
pub struct ProfileMeta {
    pub id: String, pub name: String, pub url: String,
    pub created_ms: u64, pub updated_ms: u64,
    pub nodes: u32, pub protocols: String,     // 导入摘要，如 "vless(20), trojan(3)"
    pub policy: BTreeMap<String, String>,      // Block::key() -> "base"|"subscription"
}
pub struct Store;
impl Store {
    pub fn load_profiles(p: &Paths) -> Result<Vec<ProfileMeta>>;
    pub fn save_profiles(p: &Paths, v: &[ProfileMeta]) -> Result<()>;         // 0600
    pub fn add_profile(p: &Paths, name: &str, url: &str, raw: &str,
                       nodes: u32, protocols: &str, now_ms: u64) -> Result<ProfileMeta>; // 解析失败由调用方保证不进来
    pub fn delete_profile(p: &Paths, id: &str) -> Result<()>;
    pub fn update_policy(p: &Paths, id: &str, policy: &BTreeMap<String,String>) -> Result<()>;
    pub fn touch_updated(p: &Paths, id: &str, raw: &str, nodes: u32, protocols: &str, now_ms: u64) -> Result<()>;
    pub fn snapshot_path(p: &Paths, id: &str) -> PathBuf;
    pub fn write_snapshot(p: &Paths, id: &str, raw: &str) -> Result<()>;      // 0600 原子写
    pub fn read_snapshot(p: &Paths, id: &str) -> Result<String>;
}
```
0600 用 `std::os::unix::fs::PermissionsExt`；原子写 = 同目录 tmp 文件 + `rename`。时间戳由调用方传（测试可控），生产用 `SystemTime`。

- [ ] **步骤 4：`cargo test` 全绿**
- [ ] **步骤 5：Commit** `feat: subscription store with snapshot files`

### 任务 3：subscription.rs — YAML 嗅探与导入报告

**文件：**
- 创建：`src/subscription.rs`
- 测试：`src/subscription.rs` 内 tests

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn detects_clash_yaml_and_reports_protocols() {
    // 输入含 proxies:[{type:vless},{type:vless},{type:trojan}] + proxy-groups
    // mapping.proxies 长度 3；report.total_nodes==3；protocols 含 "vless(2)"; skipped 空
}
#[test] fn invalid_yaml_is_a_structured_error() {
    // "\t不是: yaml: [" → Err，不含 panic
}
#[test] fn yaml_without_proxies_is_rejected() { /* {"mixed-port":7890} → Err("no proxies") */ }
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
pub struct ImportReport { pub total_nodes: usize, pub protocols: BTreeMap<String,usize>, pub skipped: Vec<String> }
pub struct ParsedSubscription { pub mapping: serde_yaml_ng::Mapping, pub report: ImportReport }
pub fn parse_subscription(input: &str) -> Result<ParsedSubscription>;
// 顺序：YAML(含 proxies 序列) → base64 解码后按行 parse_link → 直接按行 parse_link；全失败 → Err
fn summarize(report:&ImportReport)->String; // "vless(2), trojan(1)"
```

- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: subscription format sniffing with clash yaml`

### 任务 4：subscription.rs — base64 与分享链接解析（vless/hysteria2/vmess/anytls/trojan）

**文件：**
- 修改：`src/subscription.rs`
- 测试：同文件 tests

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn parses_base64_list_with_all_five_protocols() {
    // base64(标准+URL-safe+无 padding 三种都要解) 的五行链接 → 5 个节点；
    // 每个 mapping 断言 type/server/port 及关键字段：vmess.uuid/vmess.alterId、
    // vless.servername、trojan.password、hysteria2.obfs、anytls.password
}
#[test] fn link_names_come_from_fragment_and_are_percent_decoded() {
    // trojan://pw@h:1#🇭🇰%20HK%2001 → name=="🇭🇰 HK 01"
}
#[test] fn unsupported_scheme_and_missing_fields_are_skipped_not_fatal() {
    // ss://… 与缺 port 的 vless → skipped 2 条；其余节点保留
}
#[test] fn hostile_input_never_panics() {
    // ""、"AAAA"、1MB 随机二进制、超长单行 → 返回 Err 或全 skipped，不 panic、及时返回
}
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现** `fn parse_link(line: &str) -> Result<serde_yaml_ng::Value, String>`
  - scheme 分发；vmess=base64 JSON（`ps/add/port/id/aid/scy`，缺 ps 用 `add:port`）；其余用 `url::Url` 解析，userinfo=密码/uuid，查询参数按 mihomo 字段映射（`security=tls→tls:true,sni→servername`；`type=ws→network+ws-opts`；hysteria2 的 `obfs/obfs-password`；anytls 的 `insecure`）。必填缺失（server/port/凭据）→ Err(原因)。
- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: share-link parsers for five protocols`

### 任务 5：subscription.rs — 下载与导入流程

**文件：**
- 修改：`src/subscription.rs`
- 测试：同文件 tests

- [ ] **步骤 1：编写失败的测试**

```rust
#[tokio::test] async fn import_from_raw_stores_snapshot_and_meta() {
    // tempdir；import_from_raw(paths,"甲","https://sub.example", SAMPLE_YAML)
    // → ProfileMeta{nodes, protocols} 正确；快照内容==原文；load_profiles 出现该条
}
#[tokio::test] async fn failed_parse_leaves_no_files_behind() { /* 传垃圾 → Err；profiles.json 未创建 */ }
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
pub async fn download(url: &str) -> Result<String>;            // reqwest 15s 超时，非 2xx 带状态码报错
pub fn import_from_raw(paths:&Paths, name:&str, url:&str, raw:&str) -> Result<ProfileMeta>;
```
`import_from_raw` = parse → `Store::add_profile`。解析失败的错误信息带上 report.skipped 摘要。

- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: subscription download and import flow`

### 任务 6：merge.rs — 配置块模型与 diff

**文件：**
- 创建：`src/merge.rs`
- 测试：同文件 tests

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn block_keys_cover_the_whole_config() {
    // Block::Inbounds.keys()==["port","mixed-port","socks-port","redir-port","tproxy-port"]；
    // Block::ALL 恰好 10 项
}
#[test] fn diff_reports_equal_and_unequal_blocks() {
    // base/sub 相同 proxies → equal；不同 rules → 不等且摘要非空（如 "rules: 3 rules" vs "rules: 1 rules"）
}
#[test] fn missing_on_one_side_is_a_diff() { /* base 无 dns、sub 有 → equal==false，base 摘要=="-" */ }
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
pub enum Block { Proxies, ProxyGroups, Rules, Dns, Tun, Inbounds, LogLevel, AllowLan, Ipv6, Mode }
impl Block { pub const ALL: [Block; 10]; pub fn keys(self)->&'static [&'static str]; pub fn title(self)->&'static str; }
#[derive(Clone, Copy, PartialEq)] pub enum Source { Base, Subscription }
impl Source { pub fn label(self)->&'static str /*"base"/"subscription"*/; pub fn toggle(self)->Self; }
pub struct BlockDiff { pub block: Block, pub base: Option<String>, pub sub: Option<String>, pub equal: bool }
pub fn diff_blocks(base: &Mapping, sub: &Mapping) -> Vec<BlockDiff>;
pub fn has_diff(diffs: &[BlockDiff]) -> bool;
fn block_equal(block, base_map, sub_map) -> bool; // Inbounds 比较所有 keys；其余单 key
fn summarize(block, value: Option<&Value>) -> String; // 节点/规则计数、"present"/"-"，文案自定但确定性
```

- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: config block diff model`

### 任务 7：merge.rs — 合并、引用修补与校验

**文件：**
- 修改：`src/merge.rs`
- 测试：同文件 tests

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn merge_follows_per_block_choices() {
    // base 与 sub 各有 rules/dns；choices{Rules:Subscription, Dns:Base} → 输出各取所选
}
#[test] fn link_only_subscription_gets_default_groups() {
    // sub.mapping 只有 proxies、choices{ProxyGroups:Subscription}
    // → 生成 PROXY(select, 含 AUTO/DIRECT/全部节点) 与 AUTO(url-test)；rules 块为空列表
}
#[test] fn dangling_group_members_are_patched_with_notes() {
    // base.groups 引用 "ghost-node"（不存在）→ 合并后成员被剔除，notes 含 "ghost-node"
    //（审查重点 5：节点名 "🇭🇰 HK 01"/"a/b"/重复名 不破坏修补与去重）
}
#[test] fn rules_referencing_missing_groups_are_dropped() { /* 对应 rule 被剔除并记 note */ }
#[test] fn validate_rejects_empty_proxies_and_missing_fields() {
    // proxies 为空 → Err；节点缺 server → Err，信息含节点 name
}
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
pub fn merge(base:&Mapping, sub:&Mapping, choices:&BTreeMap<Block,Source>) -> Result<Mapping>;
pub fn patch_references(cfg:&mut Mapping) -> Vec<String>;  // 返回修补说明，逐条人可读
pub fn validate(cfg:&Mapping) -> Result<(), Vec<String>>;
pub fn default_groups(node_names:&[String]) -> Vec<Value>; // PROXY(select)+AUTO(url-test, gstatic 204, interval 300)
```
merge 按块取源组装顶层 mapping；ProxyGroups 选 Subscription 且无 groups 时用 `default_groups`。

- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: block merge with reference patching and validation`

### 任务 8：runtime.rs — 运行配置渲染与原子落盘

**文件：**
- 创建：`src/runtime.rs`
- 测试：同文件 tests（tempdir）

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn runtime_includes_controller_and_secret() {
    // render_runtime(cfg, Some("http://127.0.0.1:9090"), Some("s3cret"))
    // → yaml 字符串含 external-controller 与 secret；不带 secret 时该键不出现
}
#[test] fn write_atomic_sets_0600_and_never_leaves_tmp() {
    // 写入后权限 0o600；目录内无 *.tmp 残留；覆盖写已存在文件成功
}
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
pub fn render_runtime(cfg:&Mapping, endpoint:Option<&str>, secret:Option<&str>) -> Result<String>;
pub fn write_atomic(path:&Path, content:&str) -> Result<()>; // 0600，tmp+rename（审查重点 1）
```

- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: runtime config rendering and atomic write`

### 任务 9：api.rs — 控制器重载与按请求测速超时

**文件：**
- 修改：`src/api.rs`
- 测试：同文件 tests（新增 `#[tokio::test]`，dev 依赖 wiremock）

- [ ] **步骤 1：编写失败的测试**

```rust
#[tokio::test] async fn reload_config_puts_payload() {
    // wiremock PUT /configs 返回 204；断言 body JSON 的 payload 含 "mixed-port"
}
#[tokio::test] async fn reload_failure_carries_status_and_body() { /* 400 + body → Err 含两者 */ }
#[tokio::test] async fn proxy_delay_timeout_is_per_request() {
    // wiremock /proxies/n1/delay 延迟 800ms；proxy_delay(...,timeout_ms=100) → Err（不碰 10s 总超时）
    // timeout_ms=5000 → 成功返回 delay
}
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
pub async fn reload_config(&self, yaml_payload: &str) -> Result<()>; // PUT /configs {"payload": yaml}
pub fn secret(&self) -> Option<&str>; // 供 runtime.yaml 渲染 external-controller/secret
// proxy_delay：请求级 .timeout(Duration::from_millis(timeout_ms + 1500))
```
（`PUT /configs` 支持 payload 在任务 11 的集成测试中对 wiremock 再次验证；若实现时发现 mihomo 需要 path 方案，在此任务内改 `reload_config` 签名加备选并同步任务 11。）

- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: controller reload api and per-request delay timeout`

### 任务 10：app.rs — 确认页与订阅页状态机（纯逻辑）

**文件：**
- 修改：`src/app.rs`
- 测试：同文件 tests

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn diff_proposal_opens_pending_with_policy_prefilled() {
    // Event::DiffProposal{policy:{rules:subscription}} → pending.choices[Rules]==Subscription，cursor=0
}
#[test] fn confirm_page_keys_work() {
    // b/s/←→ 切换 cursor 块的 Source；↑↓ 移 cursor；r 切 remember；Esc 关闭不发命令；
    // Enter → Command::ConfirmSwitch{profile_id, choices, remember}
}
#[test] fn bracket_keys_cycle_profiles() {
    // 3 个 profile、sub_idx=0：'[' → SwitchProfile(s2)，']' → SwitchProfile(s1)（都从当前 id 出发循环）
}
#[test] fn input_overlay_edits_two_fields() {
    // a 打开 → 字符进 url；Tab 切 name；Enter → Command::AddProfile；Esc 关闭
}
#[test] fn external_dirty_flag_and_v_key() {
    // Event::ExternalChange → external_dirty=Some；'v' → Command::SwitchProfile(当前 id) 并清脏标记
}
#[test] fn reload_result_becomes_a_toast() {
    // ok → Success "switched"；rolled_back → Error 含 "rolled back"；error → Error 含原因
}
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现** 新类型与字段

```rust
pub struct PendingSwitch { pub profile_id:String, pub diffs:Vec<BlockDiff>, pub choices:BTreeMap<Block,Source>,
                          pub cursor:usize, pub remember:bool }
pub struct InputOverlay { pub url:String, pub name:String, pub field:InputField } // InputField::{Url,Name}
// App 新字段：profiles:Vec<ProfileMeta>, sub_idx:usize, pending:Option<PendingSwitch>,
//             input:Option<InputOverlay>, external_dirty:Option<String>
// Event 增：DiffProposal{profile_id,diffs,policy}, ProfilesUpdated(Vec<ProfileMeta>), ExternalChange{path:String},
//           ReloadResult{ok:bool, rolled_back:bool, error:Option<String>}（→ toast 成功/回滚/失败文案）
// Command 增：SwitchProfile(String), ConfirmSwitch{profile_id,choices,remember}, AddProfile{name,url},
//             UpdateProfile(String), DeleteProfile(String)
// on_key 优先级：input > pending > 全局；DeleteProfile 用 app 内 delete_armmed:Option<String> 两次 d 确认
```

- [ ] **步骤 4：全绿**
- [ ] **步骤 5：Commit** `feat: switch confirmation and subscription state machine`

### 任务 11：main.rs — profile worker（合并→校验→落盘→重载→回滚）

**文件：**
- 修改：`src/main.rs`
- 创建：`tests/profile_flow.rs`（集成测试，wiremock + tempdir）

- [ ] **步骤 1：编写失败的集成测试**

```rust
// tests/profile_flow.rs —— 组装：tempdir Paths、写 base.yaml、Store::add_profile 写快照、
// Client::new(wiremock.uri)、spawner 跑 worker、mpsc 收 Event
#[tokio::test] async fn confirm_switch_applies_and_records_policy() {
    // SwitchProfile → DiffProposal；ConfirmSwitch(默认 choices, remember=true)
    // 断言：PUT /configs 收到 payload 含订阅节点名；runtime.yaml 落盘且 0600；
    //       profiles.json 的 policy 记住了选择；收到 ReloadResult{ok:true, rolled_back:false}
}
#[tokio::test] async fn reload_failure_rolls_back_to_last_good() {
    // 第一次 PUT 204、之后 500：切第二个订阅 → ReloadResult{ok:false, rolled_back:true}；
    // PUT 被调用两次（新配置 + 回滚旧配置）
}
#[tokio::test] async fn rollback_failure_surfaces_error() {
    // PUT 恒 500 → ReloadResult{ok:false, rolled_back:false, error 含原因}（审查重点 3）
}
#[tokio::test] async fn update_profile_refetches_and_emits_profiles_updated() { /* UpdateProfile */ }
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
struct ProfileWorker { client, paths, tx, last_good: Option<String>, current: Option<String> }
async fn spawn_profile_worker(client: Arc<Client>, paths: Paths, mut rx: mpsc::UnboundedReceiver<Command>, tx: ...)
// 处理 SwitchProfile（无 diff → 直接走 apply 路径并发 ProfilesUpdated+toast 文案 "switched (no diff)"；
//                     有 diff → Event::DiffProposal，policy 从 profiles.json 预填）
//            ConfirmSwitch（merge→validate→render→write_atomic→reload_config→GET /version；
//                          失败则 last_good 回滚重发；成功则记 last_good、存 policy）
//            AddProfile（download→import_from_raw→ProfilesUpdated）
//            ConfirmSwitch 存 policy：choices 经 Block::key() 转成 profiles.json 的 String 映射
//            UpdateProfile（download→Store::touch_updated+write_snapshot→重新走 SwitchProfile 流程）
//            DeleteProfile（Store::delete_profile→ProfilesUpdated；删的是当前订阅则 toast 警示）
// 事件文案经 short_reason()；任何 store/解析错误只 toast 不 panic
```
`run()` 中 `spawn_profile_worker` 与其他任务并列启动；`execute()` 原有 command 分支保持不变。

- [ ] **步骤 4：全量 `cargo test` 绿**
- [ ] **步骤 5：Commit** `feat: profile worker with apply, health check and rollback`

### 任务 12：main.rs — 外部修改轮询与待确认流

**文件：**
- 修改：`src/main.rs`
- 修改：`tests/profile_flow.rs`（追加用例）

- [ ] **步骤 1：编写失败的测试**

```rust
#[tokio::test] async fn external_base_edit_sets_dirty_and_review_applies() {
    // 启动含 watcher 的完整栈 → 修改 tempdir/base.yaml（写入非法 YAML 也先到这一步）→
    // 收 Event::ExternalChange；发 SwitchProfile(current) → DiffProposal 展示新 diff（审查重点 4）
}
#[tokio::test] async fn invalid_external_yaml_is_reported_not_applied() {
    // base.yaml 写坏 → SwitchProfile → worker 返回带 "parse" 字样的错误事件/toast，runtime.yaml 未被改写
}
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现** `async fn spawn_config_watcher(paths, tx)`：每 1s 比对 base.yaml 与 `subscriptions/*.yaml` 的 `(mtime_nanos, size)` 签名集合，变化即发 `Event::ExternalChange`（去抖：连续两次采样一致才报）。
- [ ] **步骤 4：全量绿**
- [ ] **步骤 5：Commit** `feat: external config change polling with review flow`

### 任务 13：ui.rs — Subscriptions 页、确认页浮层、输入框与鼠标

**文件：**
- 修改：`src/ui.rs`、`src/app.rs`（鼠标偏移捕获字段）、`src/main.rs`（mouse capture、`spawn_key_reader` 转发 Mouse）
- 测试：`src/ui.rs` tests（TestBackend）

- [ ] **步骤 1：编写失败的测试**

```rust
#[test] fn subscriptions_tab_lists_profiles_and_policy() {
    // 断言可见文本含名称、节点数、protocols 摘要、"[base]"样式策略标记、高亮行
}
#[test] fn confirm_overlay_renders_blocks_choices_and_remember_hint() {
    // 每行 "rules   base ▸ subscription" 形态（选中侧可见）、"r remember"、Esc 提示
}
#[test] fn input_overlay_renders_focused_field() { // 焦点字段可见光标标记 }
#[test] fn tab_hitboxes_match_drawn_titles() { // tab_hitboxes(100) 与 draw 出的 " 1 Overview " 位置一致 }
#[test] fn add_and_update_and_delete_hints_render() { // footer 显示 a/u/d/enter/[ ] }
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
// ui.rs：pub fn tab_hitboxes(width:u16)->Vec<(std::ops::Range<u16>, Tab)>（draw_header 复用）；
//        draw_subscriptions、draw_confirm_overlay、draw_input_overlay、确认页块行渲染
// app.rs：pub subs_top: Cell<usize>、pub members_top: Cell<usize>（draw 后由 ListState/TableState::offset() 写入）
//         pub fn on_mouse(&mut self, MouseEvent) —— 左键单击：
//         y==0 → tab_hitboxes 命中换页；Subscriptions 列表行 → sub_idx=clamp(subs_top+row)；
//         Proxies 成员表行 → member_idx=clamp(members_top+row)
// main.rs：try_init 后 execute!(EnableMouseCapture)（config.mouse 才开），TerminalGuard Drop 里 Disable；
//          spawn_key_reader 把 Event::Mouse 转成 Event::Mouse(KeyEvent 之外的枚举新增变体)
```
鼠标开关读取 `TuiConfig::load`（任务 1）；`DeleteProfile` 的"再按一次 d"提示画在 footer。

- [ ] **步骤 4：全量绿**
- [ ] **步骤 5：Commit** `feat: subscriptions tab, confirm overlay, input and mouse support`

### 任务 14：并发测速池（A/Esc/进度/排序）与 T 迁移

**文件：**
- 修改：`src/main.rs`（spawn_test_pool）、`src/app.rs`、`src/ui.rs`（进度与排序渲染）、`src/api.rs`（无需新端点）
- 测试：`tests/profile_flow.rs` 或新 `tests/test_pool.rs`、`src/app.rs` tests

- [ ] **步骤 1：编写失败的测试**

```rust
// app.rs
#[test] fn a_key_starts_test_many_with_deduped_names_and_generation() {
    // 两分组共享节点 → names 去重；testing_gen 递增；url 取当前组 test_url 否则 delay_url
}
#[test] fn progress_events_update_counters_and_stale_gen_is_ignored() { /* TestProgress/TestsFinished 带 gen */ }
#[test] fn esc_cancels_and_s_toggles_sort() { /* CancelTests(gen)；sort_by_delay 切换后 apply 顺序仅视图 */ }
// 集成
#[tokio::test] async fn pool_runs_with_bounded_concurrency_and_cancel() {
    // wiremock delay 端点挂起部分请求；20 节点、concurrency=4：断言进行中并发 ≤4（wiremock 计数）；
    // CancelTests 后不再有新请求；TestsFinished{gen} 恰好一次
}
```

- [ ] **步骤 2：失败**
- [ ] **步骤 3：实现**

```rust
// Event 增：TestProgress{gen,name,result:Result<u64,String>}, TestsFinished{gen,ok,total}
// Command 增：TestMany{gen,names:Vec<String>,url:String,timeout_ms:u64}, CancelTests(u64)
// main.rs：spawn_test_pool(client, concurrency, rx, tx)——Semaphore(并发) + 每代 Arc<AtomicBool>；
//          并发数解析顺序：--concurrency > config.json.concurrency > 8
//          每个节点一个 tokio task 调 proxy_delay，完成即发 TestProgress；代被取消则停止派发并丢弃
//          旧代结果；全部结束时发 TestsFinished
// app.rs：'A'（Proxies 页）→ gen+=1、TestAll；Esc → CancelTests(testing_gen)；'s' → sort_by_delay 翻转；
//         成员渲染在 sort_by_delay 时按 (delay 有值优先, delay 升序, 原顺序) 稳定排序；标题显示 "testing (n/m)"
// 旧 Command::TestGroup 与 Event::GroupDelay 删除，相关断言改写为 TestMany（本设计批准的行为变更）
```

- [ ] **步骤 4：全量绿（56 个旧测试 + 新测试，允许任务内改写的除外）**
- [ ] **步骤 5：Commit** `feat: concurrent delay testing with cancel, progress and sort`

### 任务 15：文档与全量验证

**文件：**
- 修改：`README.md`、`README.en.md`（新功能、`~/.config/mihomo-tui/` 布局、runtime.yaml 指向说明与 0600/凭据提示、"后续优化"清单更新——划掉已完成项）
- 无代码改动

- [ ] **步骤 1：更新两份 README**（快捷键表加 `A`/`s`/`[`/`]`/`v`/鼠标；新增 "Subscriptions" 与 "订阅与配置" 小节；写明 mihomo 重启后需指向 runtime.yaml 或重新切换）
- [ ] **步骤 2：`cargo fmt`、`cargo clippy --all-targets -- -D warnings`、全量 `cargo test --locked` 全绿**
- [ ] **步骤 3：Commit** `docs: subscription management, hot reload and concurrent testing`
