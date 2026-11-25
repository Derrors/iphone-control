# Wi-Fi 控制（设计）

状态：设计，未实现。目标是手机和主机在同一个 Wi-Fi 下时，不接线也能控制手机；插上线时照旧走 USB，拔线、插线时自动换过去。相关：[架构](../architecture.md)、[关键决策](../decisions.md)（D3、D5、D10、D11、D17、D18）、[iOS 27 原生控制](ios27-native.md)、[WDA 依赖调研](wda-dependency-research.md)。

## 1. 范围

要做的：

- 已经用 USB 配对过的手机，在主机上为它打开 Wi-Fi 控制（`iphone-control wifi on`），之后拔掉线，全部工具照常可用：截图、读界面、点击、输入、启动 App、控制台画面、AI 任务。
- 同一台手机 USB 和 Wi-Fi 都在时用 USB。拔线换到 Wi-Fi、插线换回 USB，不用重启守护进程、不用重新注册 MCP，控制台页面不用刷新。
- 一次只控制一台（D11）不变；没打开 Wi-Fi 控制的手机，在局域网里出现了也不理。

分阶段（第 8 节）：

| 阶段 | 内容 |
|---|---|
| 第一版 | macOS；iOS 27 及以上、原生控制（不用 WDA）；系统 usbmuxd 列出的网络设备 |
| 第二版 | iOS 17～26（经 WDA）走 Wi-Fi；控制台的 MJPEG 画面改经守护进程 |
| 以后 | Linux 自己发现网络设备、自己保活；iOS 27 起完全不用线的首次配对 |

不做：跨网段、经路由器或 VPN 连手机（手机只接受同一网段来的连接，发现也靠组播）；按 IP 地址直连没配对过的手机；第一次配对、打开开发者模式不用线（这两步手机只在 USB 上答应，第一版仍要接一次线）。

## 2. 现状

代码里和 Wi-Fi 有关的事实：

- **只认 USB**：`ictl_device::list_devices` 把 usbmuxd 列出的网络设备过滤掉（`crates/ictl-device/src/lib.rs`，`Connection::Usb`）。`Device` 只持有 `UsbmuxdProvider`，`is_connection` 比 UDID 和 usbmuxd 编号。
- **连接换了会自动重建**：巡检发现同一个 UDID 的 usbmuxd 编号变了，就丢掉旧会话、按新编号重建（`supervise.rs` 的 `online`）。USB 和 Wi-Fi 的编号不同，所以换线在守护进程看来就是一次重新接入，这条路已经有了。
- **巡检 10 秒一次**，主循环另有 `Daemon::wake`，叫一声就马上巡检一轮。
- **iOS 27 原生控制全走隧道**：读界面（remoteAXService）、点击按键（HID）、启动结束 App（AppService）、截图（screencaptureservice）、控制台画面（HEVC 媒体流）都在 CoreDeviceProxy 隧道里，隧道外只用 lockdown 的几个服务：设备信息、锁屏状态、开发者模式、方向（springboardservices）、App 列表和安装（installation_proxy）、描述文件（misagent）、崩溃报告、日志、Web Inspector。
- **WDA 在后台启动、不带环境变量**（D18，`Tunnel::launch_in_background`）。WDA 不设 `USE_IP` 时监听手机的所有网卡：手机连着 Wi-Fi 时，8100（HTTP，没有认证）和 9100（MJPEG）**现在就能从局域网访问**，和用不用 Wi-Fi 控制无关。这违背 D3 的本意，见 4.2。
- **TLS 不校验手机**：idevice 的 lockdown TLS（`sni.rs`）对服务器证书和握手签名一律放行。USB 上无所谓；在 Wi-Fi 上，能冒充手机 IP 的人就能冒充手机，看到主机发出的内容（输入的文字、自动解锁的密码、上传的包）。见 4.1。
- **控制台**：iOS 27 的画面经守护进程（`Request::ScreenStream`），不直连手机；低版本的 MJPEG 画面原来由 viewer 自己 `Device::find` 连手机 9100（只认 USB），2026-11 起改为向守护进程要（`Request::MjpegStream`，见 5.4），守护进程现在经 USB 连 9100。
- **写操作不重放**已经有：WDA 请求开始写入后的失败归为 `WRITE_FAILED`/`AFTER_SENT`，不退到 HID 重做（`crates/ictl-wda/src/http.rs`、`client.rs`）。

## 3. 怎么连上手机

### 3.1 三条路

| 路 | 发现 | 连接 | 需要的配对记录 |
|---|---|---|---|
| A. 网络 lockdown 上开 CoreDeviceProxy | Bonjour `_apple-mobdev2._tcp` | TCP 连手机的 lockdown（62078）→ 用配对记录做 TLS → 启动 `com.apple.internal.devicecompute.CoreDeviceProxy` → 和 USB 上一样的隧道、RSD | USB 配对得到的普通配对记录；手机上打开 `EnableWifiConnections` |
| B. Remote Pairing 隧道 | Bonjour `_remotepairing._tcp` | 配对验证（X25519 + Ed25519）→ 让手机开一个隧道端口 → TLS-PSK（密钥来自配对验证）→ 隧道、RSD | 另一种配对记录（Remote Pairing），可以经 USB 建，iOS 27 起也能从手机设置里发起、不用线 |
| C. 老办法：网络 lockdown 直接开服务 | 同 A | lockdown 上逐个启动服务，不建隧道 | 同 A |

**第一版用 A**：和现在的 USB 代码几乎一样，只是 lockdown 那一段从 USB 换成网络；配对记录、隧道、RSD、HID、无障碍、媒体流全都复用。C 拿不到 iOS 17 起的隧道服务，用不上。B 留到「不用线首次配对」那一步（第 8 节以后）：它要另一套配对记录，而且隧道外没有 lockdown 服务，隧道外的那些服务都要改走 RSD 的 `*.shim.remote`。

A 的要求和代价：

- **保活（heartbeat）**：主机要一直开着 lockdown 的 `com.apple.mobile.heartbeat` 连接，手机发 `Marco`，主机回 `Polo`。没有它，手机开了服务马上关掉；保活断了，约 10 秒后这台主机在这台手机上的**所有**服务都关掉，包括别的进程的。所以一台手机、一台主机只能有一个保活，由守护进程或 usbmuxd 持有，不能每个连接各开一个。
- **macOS 的 usbmuxd 自己做发现和保活**：它经 mDNSResponder 发现手机，在设备列表里给出 `ConnectionType: Network` 的一项，并替主机保活。所以第一版在 macOS 上不用自己发现、自己保活，只要不再把网络设备过滤掉（要真机确认 usbmuxd 的保活对本进程的连接也算数，第 9 节）。
- **Linux 的 usbmuxd 不支持网络设备**。`USBMUXD_SOCKET_ADDRESS` 指向一个能列出网络设备、兼容 usbmuxd 协议的替代实现时，第一版的代码同样能用；守护进程自己发现、保活放在以后（8.4）。
- iOS 26.4 起 Bonjour 实例名里的 MAC 地址是轮换的私有地址，靠 TXT 记录里的 `identifier` 和每台已配对主机一个的 `authTag` 认手机（`authTag = HMAC-SHA256(HKDF-SHA512(HostID), identifier)` 的前 8 字节，idevice 的 `mdns::txt_record_matches` 已经实现）。这只决定「去试哪个地址」，不是认证；认证靠 4.1 的 TLS 校验。

### 3.2 隧道里还是隧道外

隧道建好以后，隧道外用的 lockdown 服务（第 2 节列的）在 Wi-Fi 上有两种走法：照旧经网络 lockdown 开（每个服务一条 TLS 连接，都依赖保活），或者经隧道里 RSD 的 `*.shim.remote`（idevice 已经支持 installation_proxy、amfi、misagent、springboardservices、崩溃报告、os_trace、afc 的 shim）。

第一版**照旧走 lockdown**：macOS 上 usbmuxd 已经在保活，改动最小，USB 和 Wi-Fi 是同一段代码。已知例外：显示开发者模式开关（AMFI）在 Wi-Fi 上手机不回答，这一步本来就只在 `setup` 里、要求接线。shim 留给 B 路和自己保活的场景。

## 4. 安全

D3 的原则不变：**不让没认证的东西出现在局域网上**。Wi-Fi 控制之后，主机和手机之间的每一条连接都走经过认证、加密的通道。

### 4.1 TLS 校验手机（Wi-Fi 的硬前提）

在 idevice 的 `create_client_config` 换掉放行一切的校验器：

1. **证书**：对端叶证书的公钥（SPKI）必须和配对记录里 `DeviceCertificate` 的公钥逐字节相同。比公钥不比整张证书，避开设备证书没有 SAN、有效期的问题。
2. **握手签名**：TLS 1.2、1.3 的签名照常用 rustls 的 `verify_tls12_signature`/`verify_tls13_signature`（ring）验证，证明对端持有那把私钥。
3. **作用于所有连接**：lockdown、CoreDeviceProxy 以及其他所有走 lockdown TLS 的服务都经过同一个 `create_client_config`，没有「先查一次、后面裸连」的空档。USB 也一样校验，免得两条路行为不同。
4. **失败**：不匹配时这次连接失败，状态显示「手机身份校验失败」，Wi-Fi 下停止尝试这台、推送通知，请用户插线确认配对；不自动重新配对。

实现方式：在锁定的 idevice 提交上打一个最小补丁（`[patch.crates-io]` 指向带补丁的提交，同 D1 的做法），同时提交给上游，合入后改回上游提交。补丁必须带测试：错误公钥、篡改签名都被拒绝。

**没合入这个补丁之前，Wi-Fi 控制不放出**（不做成「有风险但可以打开」的开关）：Wi-Fi 上会输入锁屏密码、粘贴文字，冒充的手机能拿到它们。

### 4.2 WDA 只在隧道里

- **已单独修复**（和 Wi-Fi 无关，D3 的补充）：后台启动 WDA 时在环境里带 `USE_IP=127.0.0.1`，8100、9100 只听手机的回环地址，局域网访问不到；旧版本启动的后台 WDA 重新启动一次。
- **Wi-Fi 上**（第二版）：WDA 改为只听隧道里手机一侧的地址（`USE_IP=<隧道的服务器地址>`），守护进程经隧道的用户态 TCP 连它，不再经 usbmuxd 连手机端口。隧道重建后地址可能变，变了就重启 WDA；后台启动的 WDA 接回时也先核对它听的地址。绝不回退到连手机的局域网 IP。
- `wda-bridge` 只支持 USB。

### 4.3 只理会打开了 Wi-Fi 控制的手机

- 打开是显式的、按设备的（第 5.3 节）。usbmuxd 列出的其他网络设备，守护进程不读信息、不建连接、不出现在设备列表里。
- 没打开时，即使手机自己开着「通过 Wi-Fi 显示」，守护进程也不经 Wi-Fi 连它。

### 4.4 其余不变

控制接口仍是 0600 的 Unix socket；控制台的登录、`--lan`、任务口令（D3、D17）与手机怎么连无关。

## 5. 改动

### 5.1 `ictl-device`：线路

```rust
pub enum Link { Usb, Wifi }

/// usbmuxd 列出的一项：同一台手机 USB 和 Wi-Fi 各一项，编号不同
pub struct Endpoint { pub udid: String, pub device_id: u32, pub link: Link }

pub async fn list_endpoints() -> Result<Vec<Endpoint>>;   // USB 和网络设备都列
pub async fn list_devices() -> Result<Vec<UsbDevice>>;    // 不变：只列 USB，setup、配对、debug、doctor 的 USB 检查用
pub fn watch() -> impl Stream<Item = MuxEvent>;           // usbmuxd Listen：接入、断开
```

- `Device::from_endpoint`、`Device::link()`；`is_connection` 改为比 `Endpoint`（UDID + 编号，编号已经区分了线路）。
- `Device::wifi_connections()` / `set_wifi_connections(bool)`：读写 lockdown 域 `com.apple.mobile.wireless_lockdown` 的 `EnableWifiConnections`，写完读回核对。
- 隧道：Wi-Fi 上隧道的 MTU 不用手机报的值，限制到 1280（经 Wi-Fi 上传开发者镜像这类大块数据时，大 MTU 可能卡住；要真机确认，第 9 节）。
- `connect_port` 只在 USB 上用（`wda-bridge`）；Wi-Fi 上要连手机端口一律经隧道（第二版）。

### 5.2 `ictl-daemon`：选哪台、走哪条线

- **合并**：巡检先把 `list_endpoints` 的结果按 UDID 合并，每台手机一项，记下有哪几条线。网络项只留 `wifi_udids` 里的（4.3）。`machine::pick_device` 照旧按 UDID 选台；选中后用哪条线：有 USB 用 USB，否则 Wi-Fi。
- **换线**：控制中的手机换了线，就是一次已有的「重新接入」：`online` 发现编号变了，丢掉旧隧道、HID、无障碍会话和缓存，按新线重建。排队中和执行中的操作按现在的规则失败，不重放。换线写事件日志（「改经 Wi-Fi 控制」「改经 USB 控制」），不推送通知。
- **USB 拔掉、Wi-Fi 还没出现**：usbmuxd 往往要几秒才列出网络项。正在控制的手机打开了 Wi-Fi 控制时，USB 断开后等一段时间（`ICTL_WIFI_HANDOFF_SECS`，默认 30 秒）：期间状态是「正在换到 Wi-Fi」，自动选设备时**不换到别的手机**（D11：宁可等，不换台）；超时才算离线。
- **更快发现**：`watch()` 的接入、断开事件叫醒巡检（`Daemon::wake`），不等 10 秒那一轮。Listen 连接断了就退回只靠巡检，退避后重连。
- **插回 USB**：USB 项出现时，正在控制的手机在 Wi-Fi 上就换回 USB。正在执行的请求先做完再换（等设备锁），不打断一个进行中的操作。
- **AI 任务**：任务绑定的是 UDID，换线不结束任务。换线后的下一个操作要求先看一次屏幕（和「用户交还后先看一次」同一套规则，D17）：断开期间屏幕可能变了。
- **休眠**：手机锁屏休眠后 Wi-Fi 会断断续续，可能每隔几十秒从 usbmuxd 里消失一次，连上时还会点亮屏幕。HID 媒体流开着时手机不自动锁屏（现在就是这样），有人在用时不受影响。只经 Wi-Fi 的手机消失时：状态写「手机经 Wi-Fi 连不上（可能锁屏休眠了）」；离线通知照旧在 10 分钟后发，文字说明是 Wi-Fi；巡检不因为它时有时无反复拆、建会话（消失 3 轮以内保留会话的空壳，同列设备出错的规则）。
- **持久化**：`state.json` 加 `wifi_udids`（`serde(default)`，旧文件照常读；旧版本忽略它）。

### 5.3 命令、协议、显示

- `iphone-control wifi on [设备]`：要求这台正在经 USB 连接、已配对、开发者模式已开。在手机上打开 `EnableWifiConnections`，把 UDID 加进 `wifi_udids`。手机拒绝写入时提示在 Finder 里勾选「在 Wi-Fi 网络中显示此 iPhone」。
- `iphone-control wifi off [设备]`：从 `wifi_udids` 去掉（手机不在也能做），手机在 USB 上时顺带把开关关掉。正在经 Wi-Fi 控制它时马上断开。
- `iphone-control wifi`：列出每台打开了 Wi-Fi 控制的手机、现在看不看得到、经哪条线。
- 协议（向后兼容，D10、D12）：新请求 `Wifi { udid, on }`、`WifiList`；`Snapshot` 加可选字段 `link`（`usb`/`wifi`）、`handoff`（正在换线）；`ListDevices` 的结构化数据每项加 `links`。旧客户端忽略新字段。MCP 工具不增不改，只把说明里的「通过 USB 连接」改成「通过 USB 或 Wi-Fi 连接」。
- `status`、`devices`、控制台标明线路：`经 Wi-Fi`、`USB（Wi-Fi 也可用）`。
- `doctor`：只经 Wi-Fi 连着时「USB 上没有手机」不算错误；另查 `wifi_udids` 里的手机：usbmuxd 列没列出网络项、手机上的开关、TLS 校验是否通过、保活是否正常。usbmuxd 长时间列不出一台确实在网的手机时，提示用户可以重启 usbmuxd（守护进程不替用户重启，D5）。
- `setup`：最后一步可以选「拔线后也想控制吗？」，选是就做 `wifi on`。

### 5.4 控制台

- iOS 27 的画面本来就经守护进程（`ScreenStream`），Wi-Fi 上不用改。换线时守护进程结束旧的流，页面按现有逻辑重连，新流先发关键帧。
- 换线期间（`handoff`）页面显示「正在换到 Wi-Fi / USB」，不发触摸：HID 的 `down` 在旧连接上、`move`/`up` 到了新连接上不成立。守护进程这边也拒绝：触摸序列记下开始时的连接编号，编号变了的 `move`/`up` 一律丢掉，页面重新按下。
- 低版本 MJPEG 画面（`/stream`、本机编码的 `/video`）已改为向守护进程要（`MjpegStream`，`crates/ictl-daemon/src/mjpeg.rs`），viewer 不再自己连手机。第二版只剩守护进程这一侧：经隧道连 9100（现在经 USB）。

### 5.5 iOS 17～26（第二版）

- 低于 iOS 27 一律经 WDA。Wi-Fi 上：经隧道挂开发者镜像（已有）、后台启动 WDA 时 `USE_IP` 设为隧道地址（4.2），`WdaClient` 的传输改为经隧道的用户态 TCP（`DeviceTransport` 加一种「经隧道」的连接方式，就绪检查和之后的请求用同一种）。
- 隧道重建（换线、Wi-Fi 断了又连上）后地址变了就重启 WDA（10～60 秒）。换线期间工具调用退到 HID 的那条路，在低版本上不存在，所以低版本换线期间读屏、点击都会失败，状态写明。
- 重签、安装经 Wi-Fi 能做，但上传几十 MB 的包慢得多；自动重签照常进行，时限放宽。

## 6. 对 Agent 和用户可见的差别

- 经 Wi-Fi 时每一步都比 USB 慢，建会话、截图、读界面尤其明显；画面帧率随网络波动。工具结果不变，`iphone_status` 写明经 Wi-Fi。
- 换线那几秒内的操作失败，Agent 照常看一次屏幕再继续。
- 手机锁屏休眠时可能连不上；按一下电源键或插线即可。
- 第一次配对、打开开发者模式、显示开发者模式开关仍要接线。

## 7. 不改的

- 一次只控制一台（D11）；选定的那台不在就是离线，不换台。
- 守护进程独占设备（D2）：Wi-Fi 上的隧道、HID 会话同样只有守护进程一个持有者。
- 使用系统的 usbmuxd，不重启它（D5）。
- 自动解锁的规则（D8）不变；Wi-Fi 上要 4.1 的 TLS 校验在才允许。

## 8. 分阶段与验收

### 8.0 真机探针（`debug wifi`，feature `debug-cmds`）

不改守护进程，先回答第 9 节的问题。手机：iPhone 15 Pro Max（iOS 27.x）、iPhone 18 Pro Max（iOS 27.2.x）、iPad mini 5（iPadOS 26.7.x）；主机：macOS 27。记录：

1. `EnableWifiConnections` 能否经 USB lockdown 写入、立即生效。
2. usbmuxd 列出网络项要多久；拔线后、插线后多久变化。
3. 经网络项建 CoreDeviceProxy 隧道，读树、HID 点击、截图、HEVC 画面都通；各自耗时和 USB 对比。
4. 打上 TLS 校验补丁后，USB 和 Wi-Fi 上手机出示的证书公钥都和 `DeviceCertificate` 一致；换一张证书被拒绝。
5. 本进程的连接靠 usbmuxd 的保活就够，不用自己再开。
6. WDA 设 `USE_IP=127.0.0.1` 时 USB 上照常可用、局域网访问不到 8100/9100；设成隧道地址时经隧道可用。
7. Wi-Fi 隧道里挂开发者镜像，默认 MTU 和 1280 各试一次。

### 8.1 安全前提（可以先发，和 Wi-Fi 无关）

- idevice TLS 校验补丁 + 测试；USB 全部回归通过。
- 后台启动 WDA 带 `USE_IP=127.0.0.1`；回归测试加一条：手机连着 Wi-Fi 时，从局域网访问它的 8100/9100 失败。

### 8.2 第一版：macOS、iOS 27

- `ictl-device` 的 `Link`/`Endpoint`/`watch`、`wifi_connections`；守护进程的合并、换线、等待窗口、`wifi_udids`；`wifi` 命令、协议字段、`status`/`devices`/`doctor`/控制台的显示；触摸序列绑定连接编号。
- 单测：同一台的 USB 项和网络项合并；没打开 Wi-Fi 控制的网络项被忽略；USB 断开后等待窗口内不换到另一台；插回 USB 换回；编号变化的触摸被丢掉；旧 `state.json` 照常读。
- 真机验收（`docs/dev/regression-cases.md` 加用例）：打开秒表 App → 拔线 → 同一个 MCP 会话点「开始」→ 控制台看到计时 → 插线 → 点「停止」。记录三段耗时：拔线到看到网络项、到就绪、到控制台第一帧。另测：换线中拖动、Wi-Fi 断网时不接管旁边插着线的另一台、锁屏休眠后再操作、`wifi off`。

### 8.3 第二版：iOS 17～26、控制台 MJPEG

- WDA 经隧道（5.5）、`MjpegStream`（5.4）。真机：iPad mini 5 上同样的拔线、插线回归。

### 8.4 以后

- Linux：守护进程自己浏览 `_apple-mobdev2._tcp`、按 `authTag` 认手机（iOS 26.4 以前按 Wi-Fi MAC），用 TCP 连接加自己持有的保活（每台手机一个）。IPv6 链路本地地址要带网卡编号。
- 不用线的首次配对（iOS 27 起）：Remote Pairing 配对记录和 B 路隧道，隧道外的服务改走 RSD shim。

## 9. 风险与待确认

| 风险 / 问题 | 影响 | 对策 |
|---|---|---|
| 经网络 lockdown 开 CoreDeviceProxy 在某些 iOS 版本上失败 | 第一版走不通 | 8.0 第 3 条；不通就改走 B 路建隧道（隧道以上不变），配对记录经 USB 建 |
| usbmuxd 的保活不覆盖本进程的连接 | 服务开了就被关 | 8.0 第 5 条；不够就守护进程自己开一个保活（每台手机一个，会话期间一直开着） |
| WDA 只听回环时 USB 上连不上 | 8.1 的修复不能直接上 | 8.0 第 6 条；不行就改听隧道地址，USB 上也经隧道连 WDA |
| 手机锁屏休眠时 Wi-Fi 时有时无 | 状态来回变、操作偶尔失败 | 5.2 的「休眠」规则；文字提示按电源键或插线 |
| usbmuxd 断过一次后很久不再列出这台手机 | Wi-Fi 上长时间离线 | `doctor` 识别并提示重启 usbmuxd；以后自己发现时不受影响 |
| 大块数据经 Wi-Fi 隧道卡住 | 挂镜像、安装失败 | Wi-Fi 上 MTU 1280（8.0 第 7 条） |
| idevice 补丁维护 | 升级 idevice 时要重打 | 补丁尽量小、带测试，提交上游 |
| Wi-Fi 慢 | Agent 每步更久 | 不是功能问题；`status` 写明线路，文档说明 |
