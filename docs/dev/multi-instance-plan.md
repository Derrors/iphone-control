# 多实例：一台主机上同时控制多台设备（设计说明）

状态：第 4 稿（2026-11-29），iphone-control-52 复审结论「可以按 PR 实施」（不含部署、发布批准）。各 PR 的附加合入验收见 10.5，和第 7 节表格不一致时以 10.5 为准。基线 `main` @ `e993319`（0.3.2）。第 9 节四项决定由用户确定，不变。分 6 个 PR 实施（第 7 节）。第 10 节是三轮评审意见的处理和仍待定的问题。

## 1. 目标、范围和边界

**目标**：同一台主机上，agent1 操作手机 A，agent2 操作手机 B，两边互不影响。各自的设备锁、任务、暂停和接管、WDA 恢复、看门狗都不牵连另一边。

**做法**：每台设备一个守护进程，称为一个「实例」。实例之间靠文件锁和持久记录协调，不把单个守护进程改成同时控制多台（[D11](../decisions.md#d11接多台设备时同一时间只控制一台)，理由见第 8 节）。

**不做**：
- MCP 工具不加 `device` 参数，工具列表、参数、默认输出都不变（D10）。一个 MCP 服务只连一个实例，也就只碰一台设备。
- 不做设备池调度，也不在一个控制台页面里并排显示多台。
- 不改单实例用户的使用方式：没有登记命名实例时，路径、服务名、端口、自动选设备都和现在一样。下面这些是有意的变化，单实例用户也会遇到，各自带迁移或兼容：
  - 自动解锁停用标记改成按设备（4.3）；
  - WDA 接管记录换了存放位置，默认实例照旧回填 `state.json` 里的旧记录，供降级用（4.6）；
  - `wda-bridge` 改经守护进程转发，有活跃任务时不能开（4.5）；
  - 控制台证书改成单文件证书包，登录 cookie 换成 v2，默认端口兼容 v1（4.7）；
  - 账号目录改成原子写，加了凭据代次；旧的 Anisette 状态标成「来源未知」，但照旧使用（4.2）。

**保证的边界（v1）**：
- 互斥只在同一个用户、同一套受管安装之内成立。
- 有命名实例时，数据根固定为一个目录，第一次 `instance add` 时核对并记进登记（3.2）。默认实例实际在用的数据目录和它不一致时，`instance add` 拒绝；已登记命名实例后，默认实例带别的数据目录启动也拒绝。不支持「自定义数据根加命名实例」的组合。没有命名实例时，`XDG_DATA_HOME` 等照旧生效，不受这条限制。
- root 系统服务和普通用户服务混跑不支持。`doctor` 只在有权限看到对方时报出来，看不到时报「未知」，不承诺能检出所有混跑（5.5）。

## 2. 用户看到的样子

```bash
# 默认实例照旧（现有的 iphone-control 服务，控制台 8200），先固定它控制的那台
iphone-control select 00008130-0019295E012B803A
iphone-control instance add ipad --device 00008020-000925043499002E --viewer-port 8203
#   确认默认实例固定在 00008130-…，写登记，装并启动 iphone-control@ipad、iphone-control-viewer@ipad
iphone-control instance list
#   default  derrors的iPhone  00008130-…  已认领 就绪   控制台 :8200
#   ipad     iPad             00008020-…  已认领 就绪   控制台 :8203
iphone-control --instance ipad status
iphone-control --instance ipad mcp-config
#   claude mcp add iphone-ipad -s user -- /…/iphone-control --socket /run/user/1000/iphone-control/instances/ipad/ctl.sock mcp --local
iphone-control instance remove ipad      # 停服务并确认停下后才删登记；实例数据目录保留，--purge 才删
```

agent1 用 `iphone`（默认实例），agent2 用 `iphone-ipad`。两个 MCP 服务各连各的 socket，socket 写成绝对路径。

## 3. 实例模型

### 3.1 登记是唯一的事实来源

**登记目录**：`~/.config/iphone-control/instances/`。每个命名实例一个 `<n>.env`，由 `instance add` 写，用户不该手改，文件头有注释说明。内容分两组：

- **受管键**（实例身份）：
  - `ICTL_SOCKET`：`add` 时解析好的绝对路径；
  - `ICTL_DATA_DIR`：绝对路径；
  - `IPHONE_UDID`；
  - `IPHONE_CONTROL_VIEWER_PORT`；
  - `IPHONE_CONTROL_INSTANCE_ID`：`add` 时随机生成，用于 4.7 的 cookie，同名重建也会换。
- **用户键**：`instance add` 原样保留的其他设置，比如这台设备专用的通知配置。

**实例名**：`[a-z0-9][a-z0-9-]{0,31}`，`default` 保留。launchd 标签用单独的前缀（5.1），所以 `viewer` 这类名字不会撞上现有标签。

**选择实例**：全局参数 `--instance <名字>`，或环境变量 `IPHONE_CONTROL_INSTANCE`（登记到 `env.rs`）。

**解析**：在读设置文件之前，像 #222 的 `--task-token-file` 那样，先从原始参数和真实环境认出实例名。
- 没有实例名，或者是 `default`：行为不变。
- 命名实例：按 `env` → `<n>.env` 的用户键 → 真实环境 → 命令行 的顺序合并，后面的覆盖前面的；`<n>.env` 里写成空值表示清掉共用 `env` 里的值。然后套用受管键。
- 冲突时报错并说明出处：从任何地方（命令行、真实环境、共用 `env`）拿到了和受管键不同的 `--socket`、`--data-dir`、`ICTL_SOCKET`、`ICTL_DATA_DIR`、`IPHONE_UDID`。
- 合并在启动任何线程之前完成。
- 登记里没有这个名字（也不是 4.4 的待定登记）时报错。

这样 CLI、systemd、launchd、MCP 拿到的都是登记里写好的同一个绝对 socket，不依赖调用方有没有 `XDG_RUNTIME_DIR`。

**显式 socket 优先**：命令行上显式给了 `--socket` 时，环境和设置文件里的 `IPHONE_CONTROL_INSTANCE` 不生效。命令行上同时给了 `--socket` 和 `--instance`，而且两者对不上，才报错。

**MCP 路由（本地模式）**：现在 `mcp` 读到设置文件或环境里的 `IPHONE_CONTROL_URL`，就会改走 HTTP 转接（`main.rs:407–410`），根本不连 socket；而且设置文件在 clap 之前就已经读了（`main.rs:286–293`）。所以：
- 新增 `mcp --local`。它和 #222 的 `--task-token-file` 一样，在读设置文件之前就从原始参数里认出来；读设置文件时跳过 `IPHONE_CONTROL_URL`、`IPHONE_CONTROL_CA`、`IPHONE_CONTROL_INSTANCE`，进程环境里的这几个也忽略，一律连 `--socket`。
- 命令行上显式给了 `--url` 又给 `--local`，报错。
- `mcp-config` 和 `mcp-config --instance <n>` 生成的命令都是 `<exe> --socket <绝对路径> mcp --local`（现在 `main.rs:484–497` 已经显式写 socket），不写 `--instance`。
- 控制台给 AI 任务起的 MCP 服务（`agent.rs:435–447,489`）同样用 `--socket <绝对路径> mcp --local`，子进程环境里也去掉上面这几个变量和 `IPHONE_CONTROL_MCP_URL`。
- 测试：两套实例各自生成的命令，在注入了 `IPHONE_CONTROL_URL`、`IPHONE_CONTROL_INSTANCE`（指向另一个实例）的环境和设置文件里运行，断言连的是自己的 socket。

**`IPHONE_UDID` 兼容**：
- 没有命名实例时，写在共用 `env` 里照旧有效。
- 有命名实例时，命名实例的 UDID 只取自它自己的受管键，共用 `env` 里的值被覆盖，并且必须相等或者被清掉，否则报冲突。
- 默认实例照旧可以从共用 `env` 读 `IPHONE_UDID`。它和某个命名实例相同时，`doctor` 报错，后启动的那个停在 `ClaimedElsewhere`。

每个子命令都认 `--instance`。首次运行引导、`setup`、`service` 的特殊规则见 4.5。

### 3.2 路径

| | 默认实例（不变） | 实例 `<n>` |
|---|---|---|
| socket | `$XDG_RUNTIME_DIR/iphone-control/ctl.sock` | `add` 时解析：`$XDG_RUNTIME_DIR/iphone-control/instances/<n>/ctl.sock`，没有这个变量时是 `<数据根>/instances/<n>/ctl.sock`，写进受管键 |
| 私有运行目录（`agent.rs` 的 `tokens/`、`work/`） | 现在的位置 | socket 所在目录 |
| 实例数据目录 | `~/.local/share/iphone-control` | `~/.local/share/iphone-control/instances/<n>` |
| 设置文件 | `~/.config/iphone-control/env` | `env` 加上 `instances/<n>.env`（3.1） |
| 控制台端口 | `IPHONE_CONTROL_VIEWER_PORT`，默认 8200 | 受管键 |

**数据根**（`root`）：所有跨实例的锁和按设备的共用记录都放在这里，只有这一个位置。第 2 稿里分开的 `lock_root` 和 `shared_dir` 合并成这一个，因为 1 节已经不支持自定义数据根。

- 有命名实例时，数据根必须同时等于三样东西：
  - 默认数据目录（`paths.rs:16–25` 按 `XDG_DATA_HOME` 算）；
  - 维护锁的目录（`maintenance.rs:11–19` 按 `HOME` 算）；
  - **默认守护进程实际在用的数据目录**：由它自己报告，`Request::Daemon` 的快照里新增 `data_dir`、`socket` 两个字段，不拿 CLI 自己算出的默认值去推断。
- 第一次 `instance add` 时核对这三样，任何一样对不上（比如设了和 `~/.local/share` 不同的 `XDG_DATA_HOME`）就拒绝，并说明是哪一样。核对通过后，把数据根和设置目录（`envfile.rs:18–23` 按 `XDG_CONFIG_HOME` 算）写进 `instances/registry.json`。
- 之后每个实例启动时都对照这份记录，对不上就拒绝启动。
- 没有命名实例时，这些都不检查，`XDG_*` 照旧生效。

### 3.3 哪些共用、哪些分开

| 内容 | 放在 | 说明 |
|---|---|---|
| `account/`（Apple ID 会话、签名证书私钥、Anisette 状态、凭据代次） | 共用 | 写入全部经账号锁（4.2） |
| `wda/WDA-unsigned.ipa` | 分开 | `wda_pkg.rs:46–54` 下载时用固定的 `.part` 文件名 |
| 签名暂存、`WDA-signed.ipa`、`wda_bundle` | 分开 | 暂存文件名固定（`supervise/resign.rs:247–276`）；签好的包和设备绑定（`ictl-sign/src/lib.rs:443–467`）；不跨实例复用签好的包 |
| `ddi/` | 分开 | 冷下载共用 `.part`，失败时会清缓存（`ictl-device/src/ddi.rs:69–133`，`supervise.rs:343–351,422–433`）。用户自己设的 `ICTL_DDI_DIR` 照用户配置 |
| `secrets/passcodes/<UDID>` | 共用 | 原子写（4.3） |
| `secrets/unlock-disabled/` | 共用 | 4.3 |
| `hid-open/<UDID>` | 共用，按 UDID | 4.6 |
| `wda-adopt/<UDID>.json` | 共用，按 UDID | 4.6 |
| `devices/<UDID>.lock`、`account.lock`、`account-check.lock`、`unlock.lock`、`viewer-tls.lock` | 共用 | 第 4 节 |
| `state.json` 的其余字段、`run-state.json`、`tasks/`、`logs/` | 分开 | 同第 2 稿 |
| `viewer-tokens.json`、`viewer-tokens.lock` | 共用（决定 2） | 现有锁和热加载（#219） |
| `viewer-tls/` | 共用（决定 2） | 单文件证书包（4.7） |

## 4. 互斥和安全

### 4.1 设备租约和切换协议

**锁文件**：`<root>/devices/<UDID 转大写>.lock`。
- 只新建，从不删除或替换，所以 inode 稳定；打开时带 `O_CLOEXEC`。
- 用 `flock(LOCK_EX|LOCK_NB)` 拿锁。持有者用 `pwrite` 原地改写一小段 JSON 描述自己：`instance:<n>` 或 `probe:<n>`、进程号、设备名、型号、iOS 版本。
- 这段 JSON 只是给人看的提示：可能写了一半，也可能已经过期。谁持有锁永远以 `flock` 为准。

**租约代次**：每次拿到租约，代次加一（进程内计数）。所有碰设备的工作都要先拿一个**准入凭据**才能开始：普通请求、屏幕流、MJPEG 中转、WDA 转发（4.5）、Web Inspector、安装、卸载、自动重签和它的 `spawn_blocking`，以及 `starting`、`preparing` 这些后台任务。准入凭据里记着租约代次和 UDID，凭据在，工作就算在进行中。

- 工作做完后要回到 `Inner` 写结果的，比如 `devtools.rs:202–215` 安装完成后清 App 列表，先核对代次：代次已经变了，就只丢弃结果，不改 `Inner`。
- 重签从 `supervise/resign.rs:222` 起把 `Device` 克隆进不能中途取消的 `spawn_blocking`。它照样要持有准入凭据，排空时等它结束。

**选择门闩**：守护进程里有一个独立于 `Inner` 的「选择门闩」（进程内的异步互斥）。持有它的有三类：
- 切换：从第 1 步拿到，一直持有到第 8 步；
- `PinCurrent`（4.4）；
- 任务登记：`TaskOpen` 从检查到登记完成，`SetupStep` 和 `WdaForward` 开流时的准入检查也一样。

这样：
- 切换和任务登记互相排斥。现在 `TaskOpen` 按快照单独登记（`ictl-daemon/src/lib.rs:383–392`），已经放开 `Inner` 的切换可能碰上一个刚开、却绑定旧设备的任务。
- 有切换在进行时，`PinCurrent` 直接失败，返回「切换进行中」。

**切换**（`select` 换到另一台；默认实例在自动模式下设备断开以后，选下一台时也走同一套）：
1. **拿选择门闩和新租约**。有活跃任务时照旧拒绝（#106）。新租约拿不到就报错并说出是哪个实例，原租约、原选定、原准入都不变。新目标和当前设备是同一台时，直接返回成功，不重新拿锁。
2. **关旧准入**：在 `Inner` 里把状态标成「切换中」，新来的准入请求拒绝并说「正在切换」。然后**放开 `Inner`**。现在 `select` 在持着 `Inner` 的分发路径里跑（`core/dispatch.rs:386,598–599`），持着 `Inner` 等排空会卡死：安装完成时要重新拿 `Inner`。
3. **先看长操作**：旧代次上有重签（最长 15 分钟，`RESIGN_TIMEOUT`，`supervise/resign.rs:11`）或安装在进行时，不等，直接按第 5 步退回，报出具体在忙什么，请用户稍后再切换。
4. **结束旧设备上的流**（客户端会自己重连），**有上限地排空**剩下的短操作：上限 15 秒，期间不持 `Inner`，等旧代次的准入凭据全部释放。可以取消的后台任务（`starting`、`preparing`、4.1 末尾的旁读）先 `abort`，再 `await` 到结束。现在 `core.rs:643–659` 只 `abort` 不 `await`。
5. **超时或者遇到长操作**：放掉新租约，恢复旧准入，旧租约和旧选定都不变，放开门闩，向调用方报告是哪项还在进行。已经断开的流交给客户端重连。
6. **排空完成**：拿回 `Inner`，正常关闭隧道和 HID 会话（会清掉 `hid-open` 标记）。
7. **提交前重查登记**：重读 `instances/`。这时有了 pending 或 env，而且新目标不是允许给默认实例的那台，就按第 5 步退回，再重新准备旧设备。然后**持久化选定**：`Persisted::save` 现在出错只记一条警告（`core/persist.rs:238–253`），要加一个会返回错误的版本。写失败时同样退回到旧设备，重新准备它，向调用方报错。每次成功提交都让选定代次加一。
8. 放掉旧租约，开放新准入，放开门闩。

**退出**的预算单独定：systemd 默认 `TimeoutStopSec=30`（`service.rs:389–390`），所以退出时的排空上限是 20 秒，不等长操作。重签的阻塞闭包到时还没结束，就照常退出，进程结束时内核放锁。

**设备断开**：
- 选定了的（`IPHONE_UDID`、`select`，以及有命名实例时的默认实例）：租约照旧拿着，插回来不用重抢（D11：不静默换台）。
- 自动模式（只在没有命名实例时才有）：按 D11 现在的行为，断开就结束控制。按第 2 到 8 步收尾，放掉租约，之后可以选下一台。

**守护进程退出**：走第 2、4、6 步再放租约，排空用上面的退出预算。

**命名实例**：`IPHONE_UDID` 取自受管键，没有就拒绝启动。设备被别的实例拿着时，进入新状态 `DeviceState::ClaimedElsewhere { by }`，不建隧道、不启动 WDA，每轮巡检再试一次。

**默认实例**（决定 3）：
- **每一个选择入口**做决定之前，都重读一次登记目录：自动选、`select`、`select auto`，加上 4.4 的 `PinCurrent`。读的是 `instances/` 里的 `*.env` 和 `*.pending`，只是一次 `readdir`，开销很小，不靠每轮巡检才发现。
- 有登记（含待定登记）时：
  - 只控制固定的那一台；没有固定就进入新状态 `DeviceState::NeedsSelection`，不自动选。拔插、升级、手动启动以后也一样。
  - CLI 和 MCP 的 `select auto` 都拒绝。
  - `select` 到被别的实例拿着的设备时拒绝。

**旁读**（#243，读其他接着的设备的 lockdown 信息，只用来显示）：
- 被别的实例拿着的设备不读，名字、型号、iOS 版本取自它锁文件里的提示。
- 没人认领的设备，先用 `LOCK_NB` 拿一个 `probe:` 短租约。每台的时间预算 2 秒，最多同时读 2 台，读不出来就用缓存。
- 旁读放在后台任务里，不进主队列，也不挡正在控制那台的第一个动作。
- 认领方碰到 `probe:` 持有的锁时，在后台每 200 毫秒重试一次，最多 2 秒，不挡主队列。

`devices` 和 `iphone_list_devices` 的默认格式不变（D10，#35 仍列出全部设备）。被认领的设备在行尾追加「（由实例 X 控制）」。

### 4.2 账号锁、凭据代次和每日会话检查

**账号锁** `<root>/account.lock`：凡是可能写账号目录的路径都要拿它，包括 `login`、`setup` 的登录步骤、`session()` 和 Anisette 写入、建证书、重签。

- 锁和维护锁的共享租约**跟着真正的工作走**：在同一个阻塞闭包里拿锁、做事、**同步**原子提交（临时文件、fsync、rename），然后放锁。
- 账号目录里的写入改用同步的原子写。`vendor/plume_store/src/store.rs:53–60` 现在用 `tokio::fs` 原地写，调用方超时取消以后它可能还没写完；这处改动作为 vendor 补丁记进 `THIRD_PARTY.md`。
- 外层超时只能等闭包结束，不能丢下它不管。
- 等锁有截止时间，30 秒还拿不到就报「账号正被另一个进程使用」，不无限等。
- **只有等用户输验证码时不持锁**：现在 `login` 认证途中就会刷新 Anisette（`vendor/plume_core/src/auth/account/login.rs:294–299`、`vendor/omnisette/src/remote_anisette_v3.rs:589–612`），所以不能把整个认证阶段都放在锁外。改成：
  - 每一次读写共用的 Anisette 状态，都包进一段短的持锁区：拿锁，读改写并原子提交，放锁。
  - 只有等用户输 2FA 验证码那一段不持锁。
  - 最后合并凭据时持锁，并核对凭据代次（见下）：代次在这期间变了，就重新读一遍再合并，不能用旧的覆盖新的。

**凭据代次**：同一个账号续登也会换 GS token（`vendor/plume_store/src/store.rs:152–172`），所以「Apple ID 加 team」不能当代次用。两个文件各自 rename 也不构成一次原子提交，新 token 落盘了、代次还没加就崩溃，接手的实例会沿用旧结果。所以：
- `account/commit.json` 记着 `{ generation, accounts_sha256 }`。
- 每次凭据更新，在账号锁下：先原子写 `accounts.json`，再原子写 `commit.json`（代次加一，带上新 `accounts.json` 的 SHA-256），每步都 fsync 目录。
- 读代次时同时核对哈希。`accounts.json` 的哈希和 `commit.json` 里记的不一样，说明凭据已经变了但代次没跟上（崩在两步之间）：在账号锁下把代次加一、补写 `commit.json`，再继续。凭据变了，代次一定会变，只可能多变一次，不会漏变。

**Anisette 地址**：
- 现在的 Anisette 状态里没有记地址（`vendor/omnisette/src/remote_anisette_v3.rs:91–103,255–270`），新版本把地址写进 `account/anisette.json`。
- 旧状态的来源不明，**一律标成「来源未知」**，不拿当前地址去猜，也不会因为用过一次就变成「已确认」：
  - **没有命名实例时**，作为一个明确的兼容例外，照旧按当前配置的地址使用（和现在的行为完全一样），但状态仍标「来源未知」，`doctor` 提示一句。
  - **`instance add` 之前**必须先确认或重新配置，否则拒绝添加，免得把来源不明的历史在多实例下当成已知：
    - `iphone-control login --confirm-anisette-origin`：用户确认旧状态就是用当前地址配出来的，记为已确认；
    - `iphone-control login --reprovision-anisette`：在账号锁下按当前地址重新配置。
  - 有命名实例时，来源不明的状态不发往任何地址，会话检查报「Anisette 来源未知」。
- 某个实例配置的地址和记录的不一致时：这个实例拒绝写账号，不参选会话检查的领导，也不把这种情况算成「会话失效」，`doctor` 报配置不一致。
- 重新配置以后，原来的领导可能不再符合资格（地址对不上了）。领导每一轮都先核对自己的资格，不符合就在这一轮结束后放掉领导锁，由符合的实例接手，不需要重启。

**每日会话检查**（决定 4）：
- 作为每个守护进程里一个单独的任务，不放在 `Inner` 里，也不受设备后端的门控。不能沿用 `supervise.rs:50–56,120–136,397–402`：那样原生控制或没有 `wda_bundle` 的实例可能一直拿着领导锁却从来不查。
- 账号目录里没有账号、或者 Anisette 地址对不上时，不参选领导。
- 每轮试一次 `account-check.lock`（`LOCK_NB`），拿到的就是领导。一轮检查由领导执行，状态记在 `account/session-check.json` 里（原子写），包括：
  - 检查开始时的凭据代次 `g0` 和轮次号 `r`；
  - 下次到期时间、上次结果；
  - 已经认领发通知的 `(代次, 轮次)`。
- 提交结果之前，在账号锁下重读当前代次（按上面的规则核对哈希）：不等于 `g0` 就丢弃这次结果，按新凭据重新排期，不能拿旧检查的结论覆盖新状态。
- 决定要发「会话已失效」通知时：先持久化「`(g0, r)` 已认领」，再发送。接手的领导看到已认领的就不再发。崩溃的话，这一轮可能漏发，下一次日检还会提醒；不会重复发。
- 瞬断时照旧缩短到 1 小时后再查（`supervise/resign.rs:152–185`）。
- 维护期间（拿不到维护锁的共享租约）这一轮算没有执行，到期时间不变，下一轮再试。
- 领导锁等进行中的检查收尾以后才放：守护进程退出时等它，有上限。

### 4.3 自动解锁停用标记：按设备分开，任何时候都宁多停用、不少停用

**文件**（都在 `<root>`，都由 `<root>/unlock.lock` 保护）：
- `secrets/unlock-disabled/<UDID>`：这台设备的停用标记。
- `secrets/unlock-disabled/LEGACY`：来源不明的旧标记。它存在时，**所有设备**都算停用，除非这台设备有 `<UDID>.legacy-released`。
- `secrets/unlock-disabled/<UDID>.legacy-released`：这台设备已经被明确地从 `LEGACY` 里解除。
- `logs/UNLOCK-DISABLED`（默认实例目录，`config.rs:193–195`）：给旧版本二进制看的**影子**（旧版本只认它，`ictl-unlock/src/lib.rs:141–158`）。新版本写它时，内容是 `ictl-shadow v2 <UDID>`，记的是最后一个写入它的设备。

**规则**：只要有任何一个设备标记或 `LEGACY` 存在，影子就必须存在。

所有读写都经过同一个 helper，在 `unlock.lock` 下完成。`ictl-unlock` 里的正常路径也一样：解开或确认没凑满时撤标记（`ictl-unlock/src/lib.rs:362–390`），改成调用守护进程传进来的这个 helper，不再直接 `remove_file`。

**持久性**：只 fsync 文件，不能保证掉电后新的目录项还在。所以每次新建、rename、删除之后，都要 fsync 它所在的目录（`secrets/unlock-disabled/`、`logs/`）。

**写入（停用）顺序**：
1. 写影子：临时文件、fsync、rename，再 fsync `logs/`。
2. 写设备标记：临时文件、fsync、rename，再 fsync `secrets/unlock-disabled/`。

两步都确认落盘之后，才允许输第 2 位（#25），任何一步失败都不输。中途掉电，最坏的情况是只剩影子（见下面的「孤影子」），结果是多停用。

**清除顺序**：先删设备标记，fsync 目录；再看还有没有任何设备标记或 `LEGACY`，都没有了才删影子，再 fsync `logs/`。中途掉电，最坏是影子多留着，下次启动按孤影子处理，同样是多停用。

**孤影子**：启动时，在锁下，影子内容是 `ictl-shadow v2 <UDID>`，但没有任何设备标记，也没有 `LEGACY`。这说明掉电发生在写入的两步之间，或者清除的两步之间。分不清是哪一种，就按保守处理：给影子里记的那台 UDID 写上设备标记，让它保持停用，用户需要时用 `unlock-reset --device` 解除。影子里没有 UDID 时，转成 `LEGACY`。

**升级时迁移**（启动时，在锁下）：
- 影子存在，而且内容**不是** `ictl-shadow v2`，说明是旧版本写的：可能是从没迁移过，也可能是迁移过以后降级、旧版本又写了一次。两种情况都写 `LEGACY`，并删掉所有 `*.legacy-released`。旧标记没有设备归属，所以不按当前 `passcodes/` 展开，不会漏掉旧单文件密码、以后才配置的设备、降级后才停用的设备。
- 影子内容是 `ictl-shadow v2`：有设备标记或 `LEGACY` 时不动；都没有时按孤影子处理（见上）。

**`unlock-reset`**：
- 只对确定的目标设备生效：当前控制的那台，或者 `--device` 指定的那台。目标不确定时拒绝。
- 目标被某个实例拿着时，CLI 把请求发给那个实例去做，不自己动文件。那个实例正在输这台的密码时拒绝，免得清掉进行中的保护；它自己知道是不是在输，不靠跨进程去查。
- 目标没有被任何实例拿着时，CLI 自己先拿这台设备的租约，再在 `unlock.lock` 下做；租约拿不到就拒绝。
- 实际动作是删掉目标的设备标记；`LEGACY` 存在时，写目标的 `.legacy-released`。然后按清除顺序判断要不要删影子。
- v1 **不提供** `unlock-reset --all`：要一次判断所有设备都没在输密码，就得跨进程确认，v1 不做。`LEGACY` 按设备逐台解除。

**密码文件**：
- `account.rs:185–189` 现在原地截断再写，改成写临时文件、fsync、rename。
- 旧单文件密码的认领（`core.rs:565–575`）改成写临时文件、fsync，再用 `link()` 发布到目标名：目标已存在时 `link` 会失败，所以不会覆盖刚设好的密码，也不会留下写了一半的文件。

### 4.4 实例登记

**锁序**：所有修改登记或服务的命令都按同一顺序拿锁：`registry.lock` → 必要时写升级意向（5.3）→ 维护锁。`add`、`remove` 拿维护锁的共享租约，`update` 拿独占锁。
- 守护进程从不拿 `registry.lock`。
- 持有 `registry.lock` 时，只向守护进程发不拿设备锁的请求：`Request::Daemon`，以及 `PinCurrent`（只写选定）。

**待定登记**：`instances/<n>.pending` 是一次持久的预约，内容是名字、UDID、端口、CAS 令牌、开始时间。
- 它一出现，默认实例的所有选择入口就按「有登记」处理（4.1）。已经开始的切换，在第 7 步提交前也会重查到它。
- 它只是预约，不能当成可以启动的登记：`instance check` 只认 `<n>.env`，只有 pending 时返回失败，unit 不会启动。

**CAS 令牌**：由守护进程这次启动的随机 ID 和选定代次组成。守护进程重启以后，旧令牌一律失效，不会因为代次的数字碰巧相同就被接受。

**`instance add <n> --device <UDID>`**：
1. **锁外**：用 `Request::Daemon` 读默认实例当前控制的是哪台、选定了没有、选定代次，打印出来请用户确认（`--yes` 时跳过）。
2. **拿 `registry.lock` 和维护锁的共享租约**；正在升级（有升级意向或维护锁被独占）时拒绝。**锁内重读**一遍，和第 1 步确认的不一样就放锁、报错，请用户重试。
3. 检查名字、UDID、端口：
   - UDID 不能和别的登记或待定登记相同，也不能是默认实例控制或固定的那台；
   - 端口能 bind 上，也不和别的登记或待定登记重复；
   - 不加 `--force` 时，设备还必须接在 USB 上。
  - 数据根三样核对一致（3.2）；有账号时，Anisette 来源已经确认或重新配置过（4.2）。
4. **写 `<n>.pending`**，原子写，失败就停下。
5. **CAS 固定默认实例**：发 `Request::PinCurrent { expect_udid, expect_token }`。守护进程先拿选择门闩，有切换进行中就直接失败。拿到以后，只在当前控制的、或已经固定的正是 `expect_udid`，而且令牌（启动 ID 加选定代次）和第 1 步读到的一样时，才写入选定。写入用会返回错误的持久化（4.1 第 7 步），成功后返回新令牌。
   - 默认实例没在运行、没在控制设备、或者控制的正是要给新实例的那台时：拒绝，删掉 pending，提示先 `select`。不去猜。
   - 从第 4 步写下 pending 起，选择入口都会看到它，`select auto` 和改选定都会被拒，所以第 5 步到第 6 步之间没有空档。
6. **提交**：再用 `Request::Daemon` 核对默认实例的选定代次仍等于令牌，原子写 `<n>.env`，删掉 `<n>.pending`。
7. **装 unit 并启动**守护进程和 viewer（5.1）。成功的标准：
   - **在线**（不加 `--force`）：`Request::Daemon` 报告实例名、版本对、UDID 对、已认领；viewer 的健康检查在端口上应答，报告的 socket 对。
   - **离线**（`--force`，设备不在 USB 上）：同上，但状态允许是 `Absent`（还没认领到，因为设备不在）。viewer 必须已经真正监听在这个端口上。
8. **失败回滚**：第 6 步以后失败（包括 viewer 起不来），先停这个实例的服务，确认停下以后删 unit 和 `<n>.env`；默认实例的选定保持第 5 步的结果，并打印出来。
9. **中断恢复**：
   - 只剩 pending（第 4 到 6 步之间中断）：`instance list` 和 `doctor` 会报出来，`instance add --resume <n>` 继续，`instance remove <n>` 清掉。`--resume` 从第 1 步重新走：重新确认、重新做 CAS 固定，不沿用旧令牌；重新核对端口时，也和其他 pending 里的端口比。
   - 有 env 但服务没起来：同样提示。

**`instance remove <n>`**：
1. 拿锁。
2. 停服务，**确认停下以后**（`Request::Daemon` 连不上，服务管理器报告停止）才删 unit 和 `<n>.env`。停不下就保留登记，并报错。
3. 加 `--purge` 才删实例数据目录，而且只在第 2 步确认停下以后才删。
4. 按 UDID 的共用记录不删（4.6）。

### 4.5 绕过守护进程直接碰手机的路径

**`setup`**：
- 只把**配对**和**开发者模式**（显示开关、轮询读状态）移到设备的持有者那里执行，用新请求 `Request::SetupStep { udid, step }`，`step` 只有 `Pair`、`DevModeReveal`、`DevModeStatus` 三种。
- 守护进程处理它时，在真正读写设备之前，**持着选择门闩**（4.1）核对：`udid` 是不是当前租约的 UDID；有没有活跃任务或进行中的任务登记。有就拒绝，接管中也拒绝。设备步骤做完才放门闩，所以这期间开不了新任务。不能先读一次快照再去执行。
- 设备没有持有者时，`setup` 自己拿租约，做完配对和开发者模式就**放掉**，再去调守护进程。不在持锁的同时等守护进程去拿同一把锁。
- 登录仍然在 CLI 里做（4.2）。
- 签名走现有的 `Request::Resign`，**不**给它加字段：`parse_request` 在反序列化 `Request` 之前就把顶层的 `expect_udid` 取走，放进了 `CallCtx`（`ictl-daemon/src/lib.rs:333–356`），同名字段会反序列化成 `None`。改成复用 `CallCtx.expect_udid`，由分发路径（`core/dispatch.rs:236–239`）传给 `resign_now(force, expect_udid)`。`resign_now` 在 `Inner` 里取设备的地方（`supervise/resign.rs:194–222`）核对，对不上就拒绝，不做任何注册或签名调用。
  - 测试走真实 socket 上的 JSON：带一个不对的 `expect_udid`，断言没有任何注册或签名调用（用假的签名后端计数）。
- 等就绪用现有的 `Request::Daemon`，核对 UDID。

**`wda-bridge`**：
- 改成经守护进程转发：新的流式请求 `Request::WdaForward`。v1 规则：
  - 有任何活跃任务（`running`、`paused`、`takeover`、`waiting`）时一律拒绝开流。允许在接管时开流的话，交还以后恢复的是原来的任务，原来设计的关闭条件拦不住。
  - 开流和 `TaskOpen` 都在选择门闩（4.1）下进行：`TaskOpen` 拿着门闩，先撤销所有转发、等它们都关掉，再登记任务；转发开流时拿着门闩，检查没有活跃任务，然后才放行。
  - 转发要拿 4.1 的准入凭据，切换设备时被结束。

**`debug …`**（`debug.rs:219–229`）：直接拿设备租约，拿不到就拒绝。

**停服务时的后台 WDA 清理**（`service_cmd.rs:13–19,50–64`、`detached.rs:66–95`）：
- 只扫描**本实例拥有**的接管记录（4.6 记录里的 `owner_id`，按实例 ID 认，不按可以重复使用的名字认），并且只处理拿得到租约的设备。
- 别的实例刚好停着、锁也没人拿，也不去结束它的 WDA。
- 拿到租约但没能停掉的，就地在记录里标 `cleanup_pending`；拿不到租约的，只追加一份清理意图，不改记录（4.6）。
- `instance remove` 停服务时也走这一套。

**`service` 命令**：带命名实例时，**所有** `service` 子命令都拒绝（`service_cmd.rs:50–65`），指向 `instance` 命令。

**首次运行引导**（`guide.rs:164–193,284–345`）：
- 带命名的 `--instance` 时拒绝，指向 `instance add`。
- 已有命名实例时，只配默认实例，开头说明这一点。

### 4.6 手机上的残留状态随设备交接

**「被远程查看」**（`hidmark.rs`，#186）：
- 标记在 `<root>/hid-open/<UDID>`，只有租约持有者写或删。
- 每次拿到租约以后都重读这台设备的标记，不只在启动时读一次（`core.rs:483`、`supervise.rs:359–381`）。

**接管记录** `<root>/wda-adopt/<UDID>.json`，**只在持有租约时写**，原子替换。字段：
- `pid`、`bundle`、`team`、loopback 证明；
- `owner_id`：写记录的实例的实例 ID（3.1），默认实例有固定的 ID；
- `generation`：每次改写加一；
- `cleanup_pending`。

**清理意图**：没有租约的进程（比如停服务时设备已经被别的实例拿走）不能改记录，只能追加一份 `wda-adopt/<UDID>.intent-<实例 ID>-<随机数>.json`，里面写着：
- 写意图的实例 ID；
- 它当时看到的记录 `generation`；
- 进程身份（`pid` 加 `bundle`）。

下一个持有租约的实例用比较后再处理的方式消费意图：记录的 `generation` 和进程身份都还和意图里的一样，就结束那个进程；记录已经变了（有别的实例接管、改写过），就丢弃这份意图，不碰进程。这样旧的 stop 不能覆盖已经接手的实例写下的状态。

**新的持有者接管 WDA**：

- **本实例本来就有 `wda_bundle`**（同一个实例重启或升级）：
  - 记录核验通过（进程还活着、bundle 一致、loopback 证明对得上）就照旧接上原来的进程（D18，#141、#142、#168）。
  - 核验不过：只结束能安全认出来的进程，也就是记录里的 bundle 加进程号对得上的那个；然后重新启动。
- **本实例没有 `wda_bundle`**（新登记的实例第一次认领这台设备）：现在会先因为没有 `wda_bundle` 进入 `NeedsSigning`（`supervise.rs:290–295`），根本到不了接管函数（`supervise/wda.rs:142,302–307`）。改成：
  - 记录的 `team` 等于当前账号的 team、手机上确实装着记录里的 bundle（经 installation_proxy 查）、核验也通过时，把 bundle 写进本实例的 `wda_bundle`，直接接管，并把记录的 `owner_id` 改成本实例。
  - 条件不满足就照现在的行为进入 `NeedsSigning`，请用户给这个实例签名。这不是无缝接管，文档写明。
- **`cleanup_pending` 为真**：不接管，按「来源已知、应当结束」处理：结束对应的进程，清掉这个标记，重新启动。

**显式 `stop`、`remove` 没能停掉 WDA 时**：持有租约的就地标 `cleanup_pending`，没有租约的追加清理意图（见上），交给下一个持有者处理，不会被当成可以正常接管的 WDA。

**旧记录迁移和降级兼容**：
- `state.json` 里的旧记录 `DetachedWda` 只有 UDID 和 PID（`core/persist.rs:68–73`），没有可靠的时间。规则：**新记录永远优先**；只有这台设备还没有新记录时，才在拿到它的租约以后把旧记录迁过去（bundle 取本实例的 `wda_bundle`，team 取当前账号的）。不按 `state.json` 整个文件的修改时间去比。
- **旧字段不删**：默认实例在每次改写新记录时，把它自己那台设备的 UDID 和 PID 回填到 `state.json` 的旧字段。0.3.2 的原生路径会在挂好镜像后结束旧 WDA（`supervise/native.rs:137–151`），停服务时的清理也靠这个旧 PID（`detached.rs:66–95`），回填以后，降级回 0.3.2 时这两种清理照旧能用。
- 命名实例的设备不回填：0.3.2 本来就不会启动命名实例（5.1）。降级前，`doctor` 提示先 `instance remove` 掉所有命名实例，让它们的 WDA 按上面的流程清理完。

### 4.7 控制台证书和登录会话

**证书**：
- 改成单文件证书包 `viewer-tls/bundle.pem`，里面是证书链、私钥和名字列表。它含私钥，**从创建那一刻起就是 0600**（临时文件用 `create_new` 加 `mode(0o600)` 创建，不先建再改权限）。在 `viewer-tls.lock` 下写临时文件、fsync、rename，原子替换。读者只读这一个文件，一次读到的一定是完整的一套，不会读到另一轮的私钥或者没写完的一套（`viewer/tls.rs:162–205`）。
- 为了兼容旧版本，同一把锁下照 #218 的顺序继续写那三个文件；新版本不读它们。其中 `cert.pem` 是公开的，给远端客户端复制用；`key.pem` 仍是 0600。
- 生成时取名字的并集，名字只增不减。
- 用户自己给的证书（`--tls-cert` / `--tls-key`）不受这些影响。

**热加载**：
- 每个新的 TLS 连接都看一下 `bundle.pem` 的 (修改时间, 长度, inode)，变了就解析新的。
- 解析失败就留着旧证书，下一个连接再试（最多每秒一次），一次刷新失败不会打断 HTTPS。

**对远端的影响**：自签证书名字变化时会换一张证书（私钥保持不变）。远端 `--ca` 信任的是旧的 `cert.pem`，换证以后**可能**要重新复制（`docs/task-control.md:177,207`）：会不会失效取决于客户端怎么校验，不当成协议上的定论写死。PR3 里用实际的 TLS 客户端（`task --url --ca`、`mcp_http.py` 的 `MCP_CA`、curl `--cacert`）测一次换证后的行为，按结果写进用户文档。控制台启动时和 `doctor` 里，打印当前实际在服务的证书指纹；指纹变化时记一条日志。

**登录 cookie**：
- 改成对 `iphone-control viewer session v2 instance=<n> id=<IPHONE_CONTROL_INSTANCE_ID> port=<端口>` 做 HMAC（默认实例的 `n` 是 `default`，`id` 是固定值）。
- 绑定实例 ID 以后，A 被删除、B 后来占用同一个端口时，A 的旧登录值也不会被 B 接受。
- v1 的值只在默认实例的端口上继续认。

### 4.8 其他

- 通知共用配置，每条本来就带设备名（D11）。
- 维护锁是按请求、按操作拿的共享租约（`ictl-daemon/src/lib.rs:267–295`），升级拿独占锁，等锁有上限。
- 手机上 WDA 的端口不用按实例分开；只有主机上的 TCP 监听（控制台、`wda-bridge`）要查冲突。
- `Device::install` 的进程内锁保护的是每台手机上同一个暂存文件（`ictl-device/src/lib.rs:514–523`），认领做对以后不用全局排队。

## 5. 服务、升级和配套命令

### 5.1 服务定义

- **systemd**：
  - 默认实例的 unit 文本不变。
  - 命名实例用模板 `iphone-control@.service` 和 `iphone-control-viewer@.service`：
    - `ExecCondition=<exe> instance check %i`：退出码 1 到 254 时 systemd 会跳过这个 unit，不当成失败，也不重启。旧二进制（0.3.2）不认识这个子命令，clap 以退出码 2 结束，所以 unit 被跳过。
    - 第 2 稿用的 `RestartPreventExitStatus` 不看 `ExecStartPre`，不起作用，删掉。
    - `ExecStart=<exe> --instance %i daemon`（viewer 的相应改成 `viewer`）；两行 `EnvironmentFile`：先共用 `env`，再 `instances/%i.env`。
  - `instance check` 是纯只读的：只核对登记文件在不在、受管键齐不齐，以及当前二进制认不认识这个格式。维护期间也能跑。
- **launchd**：
  - 标签用 `io.github.derrors.iphone-control.instance.<n>`，viewer 是 `….instance.<n>.viewer`，不和现有的两个标签（`service.rs:23–26`）重叠。
  - shell 包装先跑 `instance check`，失败就 `exit 0`，launchd 不再拉起。
- **Debian 和 Homebrew**：包里带模板 unit。`deploy/deb/postinst` 只打印提示，不管服务，这一点不变。

### 5.2 `instance list`

- 用 `Request::Daemon`，不拿设备锁（`core/dispatch.rs:9–18,492–495`）。
- 每行显示三种状态：进程能不能连上、有没有认领到设备、手机是否就绪。待定登记和恢复中的事务也列出来。

### 5.3 `update`：多个服务的事务

**锁序**：`registry.lock` → 升级意向 → 停 viewer → 维护锁独占。

**升级程序锁**：升级程序在整个生命周期里持有 `<root>/updater.lock`（`flock`）。恢复命令拿得到这把锁，就说明没有活着的升级程序，可以安全地清理残留的状态。

**事务日志的阶段**：`intent` → `stopping` → `swapped` → `committed` → `viewers_restored`。每次推进都原子写日志并 fsync 目录。

1. **拿 `registry.lock` 和 `updater.lock`**。先写事务日志的第一条记录（阶段 `intent`，带意向），落盘以后再写升级意向 `<root>/update-intent`。这样任何时候存在意向，都一定有日志记着它。
   - 守护进程在任务登记那一个原子步骤里检查意向：现在 `TaskOpen` 在登记之前就放掉了共享租约（`ictl-daemon/src/lib.rs:267–295,383–392`），所以意向检查必须放在登记里面，而不是只靠维护锁。有意向时拒绝新任务，说「正在准备升级」。
   - 任务登记计数在**检查意向之前**加一，在任务已经登记、`TaskList` 能看到以后才减一。`Request::Daemon` 报告这个计数，升级程序等到每个实例都是 0，再去查任务。这样卡在检查和登记之间的 `TaskOpen` 一定会被查到。
2. **冻结清单**：默认实例加上全部登记实例，各自记下：
   - unit 或 plist 装没装、启用没有、在不在运行；
   - socket、端口、`ExecStart` 里的二进制路径。

   只纳入二进制路径和这次要替换的相同的服务。另外**保留**现在对「没有登记的自定义 socket 上有守护进程在应答」的检查，有就拒绝。
3. **查任务**：这时还没拿独占锁，`TaskList` 能用。逐个实例查，任何一个实例有活跃任务就拒绝，删掉意向；`--yes` 也不跳过。服务在运行、socket 却连不上的，同样拒绝。
4. **推进日志到 `stopping`**：写入清单、各服务原来的状态，以及**提交后要恢复哪些 viewer**。写成功之后才能停任何服务。
5. **停所有 viewer**。viewer 进程持有维护锁的共享租约，不停下来，独占锁拿不到。
6. **拿维护锁的独占锁**（有上限）。拿不到就按恢复里「还没提交」的规则退回：恢复 viewer，删掉意向，结束事务，报告是谁拿着锁。
7. **停所有守护进程**，换二进制，日志推进到 `swapped`，再启动原来在运行的守护进程。
8. **验证**：在独占期间只能验进程本身。逐个用 `Request::Daemon` 核对：进程身份（实例名）、版本、socket、固定的 UDID 配置。不验设备状态：独占期间的 `Absent` 只是启动时的初始快照（`core.rs:498–508`、`supervise.rs:48–80`），不代表 USB 上真的没有设备。
9. **全部通过就提交**：日志推进到 `committed`，放开独占锁，删掉意向。
10. **提交之后启动 viewer**，恢复到原来的状态（采纳第 2 轮的建议）。要恢复哪些 viewer，第 4 步已经写进了日志，所以提交以后、第一个 viewer 起来之前被杀，恢复命令也知道还要起哪些。全部起来后日志推进到 `viewers_restored`。viewer 起不来时不回滚，报「二进制已升级，viewer X 没有恢复」，日志停在 `committed`，可以用恢复命令单独重试。分开测「端口被占」这种失败。
11. **放锁以后另外检查设备**：各实例的设备是不是真的就绪。这一步只报告结果。

**恢复**：保留现有的命令 `iphone-control update recover`（`main.rs:473`），扩展它。先拿 `registry.lock` 和 `updater.lock`，拿不到 `updater.lock` 说明升级程序还活着，直接退出。然后按日志阶段处理：
- **有意向、没有日志，或者日志停在 `intent`**：还没停任何服务。删掉意向，结束事务，什么都不用恢复。现在 `recover` 没有日志就直接返回（`update/transaction.rs:685–688`），意向会一直留着、挡住所有新任务，这里要补上。
- **`stopping` 或 `swapped`（还没提交）**：允许回滚。先停新版本的守护进程，再把旧文件换回来，然后按清单恢复原来的服务集合和启用状态，最后删掉意向（`update/transaction.rs:278–285,348–369,406–428`）。
- **`committed`**：不回滚，只按日志把还没恢复的 viewer 起来。
- **`viewers_restored`**：什么都不做，结束事务。

几点约定：
- 回滚由升级程序自己执行；升级程序中途被杀时，由 `update recover` 执行。
- 守护进程启动时发现未完成的事务，只报告并提示运行 `update recover`，自己不动手。
- 恢复是幂等的，可以重复执行。恢复失败时，日志留着，`status` 和 `doctor` 报出来，并给出手动恢复的命令。

**APT 和 Homebrew**（`update.rs:491–506`、`update/channel.rs:105–145`）：
- 第 1 到 7 步里除了「换二进制」都照做：同样要先拿到维护锁的独占锁，才能停守护进程、交给包管理器。日志多一个阶段 `package`，记着包管理器的命令和要安装的版本。
- 包管理器完成以后，由本 CLI 按冻结的清单恢复它管的服务，并逐个确认版本。这条路径没有二进制回滚（受管安装那套回滚不适用），文档写明。
- 包管理器中途被打断时，`update recover` 读到 `package` 阶段，先看实际装上的是哪个版本：
  - 是目标版本，就按清单恢复服务并验证，然后提交；
  - 不是，就报告实际版本和需要用户手动运行的包管理器命令，服务照清单恢复到原来的状态。
- 外部的降级（用户直接用 apt 或 brew 装回旧版本）不经过这些步骤。启动门禁（5.1）只能保证旧二进制不把命名 unit 跑起来，不能替代对已经在运行的进程的停机协调；文档写明。

**发布门禁**：
- PR4（开放 `instance add`）和 PR5（`update` 支持多服务）必须在**同一个版本**里发布。
- 如果合并顺序先 PR4 后 PR5，在 PR5 合并之前，`update` 检测到命名实例时直接拒绝，提示先 `instance remove`。
- 从 0.3.2 升级上来时还没有命名实例，走现在的单服务路径。

### 5.4 `mcp-config`

命令里写绝对 `--socket`（3.1）。命名实例的 MCP 服务名是 `iphone-<n>`。

### 5.5 `status`、`doctor`

- `status` 不带参数时输出格式不变（D10），`--all` 按实例逐个显示。
- `doctor` 新增的检查：
  - 两个实例的 UDID 或端口相同；
  - 有命名实例时，共用 `env` 里写着冲突的 `IPHONE_UDID`；
  - Anisette 来源未知，或者某个实例的地址和账号里记的不一致；
  - 有待定登记、未完成的升级事务、`cleanup_pending`；
  - 有 `LEGACY` 停用标记（提示用 `unlock-reset --device`）；
  - root 和普通用户混跑：有权限看到对方时报错，看不到时报「未知」；
  - 有命名实例时，提示不要降级到 0.3.2。

## 6. 不变的东西

- MCP 工具、参数、默认输出不变（D10）。
- 没有命名实例时，默认实例的行为不变；1 节列出的两处例外都带迁移。
- 一个守护进程只控制一台设备（D11 的主体）。任务、队列、`run-state` 按实例隔离（D17），不做跨实例的任务管理。

## 7. PR 拆分和测试

**每个 PR 照常过**：
- 逐包 `cargo fmt --check`；
- Rust 1.99.1 的 clippy `-D warnings`，两条 `cargo doc`；
- `cargo test --workspace --all-features --locked`；
- MSRV 1.89 的 `cargo check`；
- `check_regression_cases.py`；
- 真机回归。

macOS 部分靠 CI，加上用户在另一台 Mac 上按 `docs/dev/macos-verification.md` 测。改到 D11、D17、D18 行为的 PR，各自同步更新 `decisions.md`、`architecture.md` 和回归用例。

**竞态的屏障和故障注入**：下面这些竞态，单测都用测试专用的屏障（`#[cfg(test)]` 的钩子），在指定的点停住、交错执行或注入失败：
- 切换卡在安装的最后一步、遇到正在重签、持久化选定失败；
- 一个 `select` 已经进入切换、放开了 `Inner`，这时写 pending、做 CAS：CAS 要失败，或者切换在提交点被 pending 挡回；
- CAS 之后、写登记之前，有 `select auto` 或改选定；
- 切换进行中有一个不带 App 范围的 `TaskOpen`；
- 停用标记写入和 `unlock-reset` 交错；用掉电模型在每个文件和目录的提交点中断，包括孤影子；
- 同一个账号续登换了 token，旧的检查结果要提交；`accounts.json` 已经写了、`commit.json` 还没写时崩溃；
- 拿不到租约的 stop 写了清理意图，期间另一个实例已经接管；
- `TaskOpen` 卡在升级预检的边界上（计数在检查意向之前就加了）；
- 升级程序在写意向后、提交后启动第一个 viewer 之前、回滚途中、包管理器中途各被杀一次。

**双实例真机测试的统一要求**：
- 每一路固定自己的 socket、`MCP_CMD`（或 HTTP endpoint）、`SHOTS_DIR`。
- 开始操作前先断言控制的正是预期的 UDID（`tests/device/run.py:116–123,180–193`、`device.py:129–136`）。
- 延迟以同一台机器上单实例的基线为准，阈值事先定好（`compare.py`）。

| PR | 内容 | 单测（含屏障和故障注入） | 真机和故障验收 |
|---|---|---|---|
| 1 | 4.3 全部（统一 helper、写入和清除的顺序和目录 fsync、`LEGACY`、影子标记、孤影子、迁移、按设备 reset，不提供 `--all`；`ictl-unlock` 改用 helper；密码文件原子写和 `link` 发布）；4.2 的账号锁、同步原子写（vendor 补丁）、`commit.json` 凭据代次、Anisette 的短持锁区、Anisette 来源标记 | 假锁屏键盘加掉电模型：在每个文件和目录的提交点中断后，确认是多停用而不是少停用；孤影子；reset 和停用写入交错；`.migrated` 之后降级、旧版本又写标记，再升级回来变成 `LEGACY`；旧密码发布不覆盖；登录或续登和检查交错时按代次丢弃旧结果；`accounts.json` 和 `commit.json` 之间崩溃后代次照样变；等锁到截止时间 | 不故意输错真密码。真实安装上放一个旧标记，升级后两台都停用；`unlock-reset --device` 一台以后，另一台仍停用；`run.py` 默认步骤 |
| 2 | 3.1 和 3.2 的登记解析和冲突检查、显式 socket 优先、`mcp --local`、数据根核对（`Request::Daemon` 报告 `data_dir`、`socket`）；4.1 的租约、代次和准入、选择门闩、切换协议（长操作直接退回、提交前重查登记）、断开语义、`ClaimedElsewhere`、`NeedsSelection`、旁读；4.6 的残留交接、接管记录（`owner_id`、`generation`）、清理意图、空实例接管、旧记录迁移和回填；4.2 的会话检查领导。不提供 `instance` 命令 | 默认实例的路径逐字不变；名字、冲突、空值覆盖；注入 `IPHONE_CONTROL_URL`、`IPHONE_CONTROL_INSTANCE` 以后 `mcp --local` 仍连自己的 socket；租约和大小写；同一台复选不重拿锁；遇到重签时立即退回，短操作超时后保持原样；`abort` 之后有 `await`；持久化失败回到旧设备；切换和 `TaskOpen` 互斥；已经进入的切换在提交点被 pending 挡回；新记录优先于旧记录；清理意图在记录变了以后被丢弃；领导的唯一性、接手、资格变化时放锁、按代次和轮次去重通知、维护期间不算执行 | 只在**隔离环境**里测：停掉真实服务，用单独的 `HOME` 和 `XDG_*` 手动起两个实例。测试项：两路并发跑 `run.py` 和 `scenarios.py`；大小写不同的 UDID 争同一台；拔插、并发切换；安装或网页检查进行中切换；HID 开着时 `kill -9`，等着的实例接手后清掉残留；空实例对已有 WDA 的设备接管或进入 `NeedsSigning`；原生控制的领导退出后由别的实例接手；A 的任务口令在 B 被拒；两边同时开任务；A 被接管不影响 B；#243：一台的旁读不拖慢另一台的第一个动作 |
| 3 | 4.5 全部（`SetupStep` 在门闩下准入、`Resign` 复用 `CallCtx.expect_udid`、`WdaForward` 和任务互斥、`debug`、按 `owner_id` 清理、命名实例下的 `service` 和引导、`agent.rs` 改用 `mcp --local`）；4.7（0600 的证书包、热加载、cookie v2 绑定实例 ID、换证后用实际客户端测 `--ca`） | `SetupStep` 的 UDID 不对或有任务时拒绝；在真实 socket 上发带错误 `expect_udid` 的 `Resign` JSON，没有任何注册或签名调用；转发和 `TaskOpen` 的串行；只清本实例的记录；证书包和旧三文件并发写；热加载失败保留旧证书；删掉实例再复用端口以后旧 cookie 失效 | 在隔离环境里：bridge 开着时切换设备；任务进行中开不了 bridge；`setup` 在设备被持有时经持有者配对；两个控制台同时冷启动，用同一个 CA 访问；改了名字以后另一个控制台不重启也换了证书 |
| 4 | 4.4 登记（pending、CAS 令牌、在线和离线的成功条件、回滚、`--resume`、确认停下后才 remove 和 purge）；5.1 的服务定义和 `ExecCondition`；`instance list/check`；`mcp-config`；`status --all`、`doctor`；5.3 的发布门禁（PR5 合并前 `update` 遇到命名实例就拒绝） | unit 和 plist 文本；并发 `add` 和 `remove`；CAS 失败；pending 中断后恢复；viewer 起不来时回滚；`instance check` 只读；门禁 | 真实 systemd 上验证 `ExecCondition` 和 0.3.2 二进制被跳过；`instance add ipad` 后两个实例开机自启；默认实例没在运行时 `add` 拒绝；`--force` 离线登记；`remove` 停不下时保留登记；8200 和 8203 用同一个令牌各自登录 |
| 5 | 5.3 全部（`updater.lock`、日志阶段、意向和任务登记计数、冻结清单、锁序、只验进程、提交后再起 viewer、扩展的 `update recover`、APT 和 Homebrew 也拿独占锁并有 `package` 阶段、外部降级的说明）；去掉 PR4 的门禁 | 假 systemctl 和 launchctl：停着或禁用的保持原样；只有非发起实例有任务时拒绝；`TaskOpen` 卡在预检边界；在写意向后、停机前、换二进制后、验证中、提交后第一个 viewer 起来前、回滚中、包管理器中途各杀一次升级程序，恢复后一致，恢复可以重复执行；升级程序还活着时 `update recover` 不动手 | 两个实例运行时升级；一个实例原来停着，升级后仍停着；从 0.3.2 首次升级；在两个支持多实例的版本之间回滚；viewer 端口被占时只报告、不回滚 |
| 6 | 用户文档（中英）、`env.example`、FAQ；`tests/device/instances.py` 把 PR2 到 PR5 的场景固定成脚本；回归用例补齐 | `check_regression_cases.py` | `instances.py` 全部场景，包括一边 WDA 卡死（#103）时另一边按基线和阈值判断 |

实施时 PR1 拆成两个，互不依赖：**PR1a** 是 4.3 全部加密码文件的原子写（#231）；**PR1b** 是 4.2 的账号部分（账号锁、同步原子写、`commit.json`、Anisette 的短持锁区和来源记录，含 10.5 的 PR1 验收）。

PR2 和 PR3 合并以后、PR4 合并之前，代码里没有 `instance` 命令，多实例只能在隔离环境里手动配置，文档不宣传。

## 8. 为什么不做单进程多设备

- 要把 `Inner` 里的会话、锁、缓存，以及巡检、WDA 启动和恢复、原生准备、重签、解锁、任务路由、控制台全部按设备拆开，等于重写守护进程。
- 一个请求带错设备就是误操作；分进程时，一个实例在结构上碰不到别的设备。
- 故障会互相拖累：WDA 卡死、看门狗 `exit(1)`、axauditd 卡住，都会影响同一进程里的其他设备。
- 要给 MCP 工具加设备参数，agent 用起来更复杂。

出现这类需求时再重新评估：一个页面看所有设备、设备池调度，或者设备多到每台一个进程负担明显。

## 9. 已确定的问题（用户，2026-11-28）

1. **路径和服务名**：用 `instances/<n>/` 和 `iphone-control@<n>`（3.2、5.1）。launchd 标签是另外定的前缀。
2. **控制台令牌**：所有实例共用一套，证书也共用（3.3、4.7）。
3. **默认实例**：有命名实例时也必须固定一台设备，不再自动选（4.1）。
4. **每日 Apple ID 会话检查**：由拿到 `account-check.lock` 的实例做，持锁的实例退出后别的实例接手（4.2）。

## 10. 评审意见的处理

### 10.1 第 1 轮（第 2 稿已处理）

「必须改」9 条和「建议改」全部采纳。开发者镜像和签好的 WDA 选了「不共享」，没有做按版本寻址。

### 10.2 第 2 轮

| 意见 | 处理 | 落在 |
|---|---|---|
| 必须 1：切换要有代次，准入和收尾不能持着 `Inner` 等；重签也要纳入；持久化提交要能报错；同一台复选；固定和自动模式下断开的不同语义 | 采纳 | 4.1 |
| 必须 2：登记要原子，`--force` 的成功条件 | 采纳：pending 预约、锁外确认锁内重读、CAS 令牌、在线和离线的成功条件、viewer 失败回滚、确认停下后才 remove 和 purge | 4.4 |
| 必须 3：唯一的安装上下文，MCP 路由用绝对 socket | 采纳：登记作为唯一来源，受管键；数据根唯一，不支持自定义数据根加命名实例；`mcp-config` 输出绝对 socket；派生 MCP 去掉路由环境；`IPHONE_UDID` 的兼容规则 | 1、3.1、3.2 |
| 必须 4：`setup`、bridge、`service` 接入 D17 和租约 | 采纳：`SetupStep` 必带 UDID 并在执行前核对；`Resign.expect_udid` 真正生效；CLI 先放租约再调守护进程；有活跃任务时 bridge 一律拒绝，并和 `TaskOpen` 串行；`service` 子命令全部拒绝 | 4.5 |
| 必须 5：后台 WDA 要有可启动的配置和待清理语义 | 采纳：空实例按 team 和已装的 bundle 接管，否则进入 `NeedsSigning`；`cleanup_pending`；按 `owner` 清理；只结束能认出来的进程；惰性迁移、不覆盖更新的记录；降级策略 | 4.6 |
| 必须 6：解锁兼容标记要 fail closed | 采纳：统一的锁和 helper；先影子后设备、先设备后影子；`LEGACY` 不按设备展开；影子内容带标识，能认出降级后旧版本又写的；reset 发给持有者，输密码时拒绝；密码用 `link` 发布 | 4.3 |
| 必须 7：TLS 只能读到已提交的完整一套 | 采纳：单文件证书包；失败保留旧证书；写明对远端 `--ca` 的影响；cookie 绑定实例 ID | 4.7 |
| 必须 8：账号要有真实的写入边界和凭据版本 | 采纳：同步原子提交，锁随工作；等锁有截止；2FA 分两段；凭据代次计数；Anisette 来源不明时的处理；地址不符不参选、也不算会话失效；维护期间不算执行；收尾后才放领导锁 | 4.2 |
| 必须 9：升级顺序要能执行，写明回滚执行者和发布门禁 | 采纳：意向检查放进任务登记；先停 viewer 再拿独占锁；只验进程；独立恢复器；`ExecCondition`；保留自定义 socket 的检查；发布门禁；APT 和 Homebrew 由本 CLI 恢复；外部降级的说明 | 5.1、5.3 |
| 建议：旁读预算 | 采纳：每台 2 秒、最多 2 台并发、放后台任务、`probe:` 不挡主队列，加 #243 的测试 | 4.1、7 |
| 建议：`setup` 只移配对和开发者模式 | 采纳 | 4.5 |
| 建议：只验守护进程就提交 | 采纳：viewer 在提交后启动，失败只报告、可单独重试 | 5.3 |
| 建议：root 和用户混跑看不到时报「未知」；通知按代次和轮次去重 | 采纳 | 1、4.2、5.5 |
| 可以不改：默认端口的 v1 cookie、通知不保证恰好一次、缓存不共享、命名空间和队列模型 | 保留 | — |
| 建议：竞态加屏障和故障注入 | 采纳 | 7 |

### 10.3 第 3 轮（第 4 稿）

复审结论：主线可以定下，补清下面这些契约后按 PR 实施。

| 意见 | 处理 | 落在 |
|---|---|---|
| 必须 1：绝对 socket 要真正固定成本地 MCP 路由；默认守护进程实际的数据根怎么核对 | 采纳：显式 `--socket` 压过环境里的实例名；新增 `mcp --local`，在读设置文件之前认出来，跳过远程路由的变量；生成的命令和 `agent.rs` 都用它；`Request::Daemon` 报告 `data_dir`、`socket`，`add` 时核对三样，记进 `registry.json`；没有命名实例时不影响 `XDG_*` | 3.1、3.2 |
| 必须 2：预约要挡住已经开始的切换，任务登记也要纳入 | 采纳：选择门闩，切换、`PinCurrent`、`TaskOpen`、`SetupStep`、`WdaForward` 开流都在它下面；切换在提交点重查登记；CAS 令牌带启动 ID；`--resume` 重新固定；pending 不可启动；恢复时也比其他 pending 的端口 | 4.1、4.4、4.5 |
| 必须 3：`Resign.expect_udid` 要按实际的协议解析 | 采纳：复用 `CallCtx.expect_udid`，不加同名字段；用真实 socket 上的 JSON 测；`SetupStep` 在门闩下准入 | 4.5 |
| 必须 4：拿不到租约时不能改记录 | 采纳：记录只在持有租约时写；没有租约的写清理意图，由下一个持有者按 `generation` 和进程身份消费；按实例 ID 认归属；`remove` 走这套；新记录优先；默认实例回填旧字段，0.3.2 的清理照旧能用 | 4.5、4.6 |
| 必须 5：掉电持久性和孤影子 | 采纳：每次改目录项都 fsync 目录；影子带 UDID；孤影子保守地停用它记的那台；v1 不提供 `--all`；没有持有者的 reset 由 CLI 先拿租约；掉电模型测试 | 4.3、7 |
| 必须 6：凭据代次、2FA、Anisette 来源要按实际的提交边界 | 采纳：`commit.json` 记代次和 `accounts.json` 的哈希，对不上就补加代次；Anisette 状态的每次读写都放进短持锁区，只有等验证码时不持锁；旧状态一律标「来源未知」，单实例作为兼容例外照旧使用，`instance add` 之前必须确认或重新配置；资格变化时领导自己放锁 | 4.2、4.4 |
| 必须 7：升级要覆盖写意向前后和提交后的恢复 | 采纳：`updater.lock`；先写日志再写意向；日志阶段 `intent`、`stopping`、`swapped`、`committed`、`viewers_restored`，要恢复的 viewer 在停机前就记下；扩展现有的 `update recover`；APT 和 Homebrew 也拿独占锁，有 `package` 阶段和中断后的恢复；任务登记计数从检查意向之前算起 | 5.3、7 |
| 建议：120 秒不要写成「等于重签时长」，切换和退出分开定预算 | 采纳：遇到重签或安装直接退回并报原因；短操作等 15 秒；退出 20 秒（在 `TimeoutStopSec=30` 之内） | 4.1 |
| 建议：证书包 0600、`cert.pem` 是公开的；换证后 `--ca` 的行为用实际客户端测；1 节和 6 节写全有意的变化 | 采纳 | 1、4.7 |
| 可以不改：长期本机 CA 不进 v1；缓存不共享、默认端口兼容 v1 cookie、只验守护进程就提交、混跑报「未知」 | 保留 | — |

### 10.4 第 3 稿遗留问题的结论（按第 4 轮评审建议，用户可推翻）

1. **`instance add` 前的 Anisette 来源**：默认提示 `login --reprovision-anisette`。只有明确知道旧状态就是当前地址生成的，才用 `--confirm-anisette-origin`。没有账号、没有旧状态时不多这一步。
2. **切换遇到长操作**：v1 不做自动排队切换。立即报出在忙什么，保留原设备和选定，由用户重试，不在任务或界面已经变了以后悄悄切到另一台。

### 10.5 第 4 轮：各 PR 的附加合入验收

| PR | 要求 | 依据 |
|---|---|---|
| 1 | Anisette 来源确认绑定**实际配对状态**，不只绑地址：`state.plist` 和来源记录放进同一个原子记录，或者来源记录里带 `state.plist` 的摘要；摘要对不上按「来源未知」处理，多实例下不发送。换地址中途中断不能留下「B 的状态配 A 的元数据」 | `commit.json` 的哈希只覆盖 `accounts.json` |
| 1 | 等 2FA 验证码之后，先重读共享的 Anisette 状态，再决定提交什么，不回写暂停前的缓存 | `vendor/omnisette/src/remote_anisette_v3.rs:589–612` |
| 1 | `unlock-reset` 在 PR1 只经现有守护进程执行；「没有持有者时 CLI 先拿租约直接写」依赖 PR2 的设备租约，推迟到 PR2 开放 | 4.3 |
| 2、4 | 已运行的默认守护进程、CLI、unit 必须找到**同一份登记目录**，不只核对数据根：在固定的维护根里放登记锚点，或者 v1 在命名模式下拒绝不同的配置目录。无命名实例时继续兼容 `XDG_CONFIG_HOME`。验收：CLI 和守护进程用不同的 `XDG_CONFIG_HOME`，必须拒绝或仍用同一份登记；5.1 unit 里写死的 `%h/.config` 要和这条规则一致 | `envfile.rs:18–23` |
| 2、3 | 锁序固定为选择门闩先于 `Inner` | 4.1 |
| 2、3 | 退出预算到了，还在跑的阻塞闭包不能让设备租约的 FD 先释放：工作持有租约的引用，或者等进程真正退出、由内核放锁 | 4.1 |
| 2、3 | WDA 接管和清理的比较要区分「记录的代次」和「设备上进程的实际创建身份」：`pid + bundle` 不足以证明还是原来那个 loopback 进程（PID 复用、设备重启后）。没有可信证明就不接管，走正常重建。迁移时只有和旧 `loopback_wdas` 精确匹配的记录才算带着旧版证明 | `core/persist.rs:142–146`、`ictl-device/src/lib.rs:862–871` |
| 5 | `committed` 阶段的恢复要清掉残留的 `update-intent`，并解除 `maintenance.pending` 的全局阻断，然后才恢复 viewer。断言：在「落盘 `committed` 后、删意向前」和「删意向后、起 viewer 前」被杀，`update recover` 之后 MCP 能开新任务，viewer 保留可重试 | `crates/ictl-daemon/src/maintenance.rs:65–79,108–117` |
| 5 | `package` 阶段的恢复要先确认包管理器进程已经退出、不再持有包锁：杀掉升级程序不等于杀掉它起的子进程，不能一看到目标版本就启动服务；状态不明时保持「待恢复」 | `update/channel.rs:161` |
