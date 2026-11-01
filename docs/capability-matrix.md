# 能力对照表

把 toolkit 现有的每项能力，对应到 D 方案里由谁实现。

状态说明：
- **已有**：上游 crate 有现成 API，按源码核实过，接上即可（仍需真机验证 iOS 27）
- **需修**：上游有，但有已知缺陷要改
- **需自写**：上游没有，或是我们自己的业务逻辑

> 2026-11-01：表中「已有/需修/需自写」各项均已编码实现，状态和真机待验证项见 [roadmap 的完成情况](roadmap.md#完成情况)。

上游版本基线：idevice 0.1.68（现锁上游 master `89702099`，见 decisions D1）、Impactor 2.7.2（plume_core / plume_utils / omnisette）、apple-codesign 0.29.0、rmcp 3.8.0。

## 1. 设备基础

| 能力 | toolkit 现在靠 | D 方案实现 | 状态 | 风险 / 待验证 |
|---|---|---|---|---|
| 发现设备、热插拔 | usbmuxd + supervisor 轮询 | `idevice::usbmuxd::UsbmuxdConnection::{get_devices, listen}` + sysfs 检查 | 已有 + 需自写（看护逻辑） | 容器无 udev，必须保留"重启 usbmuxd"兜底 |
| 配对、信任 | `idevicepair`（libimobiledevice） | idevice `pair` feature；配对记录通过 usbmuxd `save_pair_record` | 已有 | 首次配对流程待验证 |
| 显示开发者模式开关 | `pymobiledevice3 amfi reveal-developer-mode` | idevice `amfi` | 已有 | — |
| iOS 17+ 隧道（免 root） | `ios tunnel start --userspace` / pmd3 进程内隧道 | `CoreDeviceProxy::create_software_tunnel()`（jktcp）+ RSD 握手 | 已有 | README 自述 "Naive" TCP 栈；长时间 WDA 流量、大截图下的稳定性和吞吐待验证 |
| 挂载开发者镜像（iOS 27 Cryptex） | `pymobiledevice3 mounter auto-mount` | `cryptexd::install_ddi()`、`installed_ddi()`；读 `read-personalization-id` + `get-nonce`，`TSSRequest::add_cryptex1_tags` 向 `gs.apple.com` 签票，再 `install` | 已有（流程完整） + 需自写（DDI 下载） | `tss` feature 标注 "Partial support"；`install_ddi` 需要调用方提供解包好的 DDI `Restore` 目录（`Cryptex1Assets::load`），**下载 DDI 要自己做**。最高风险项 |
| 安装 IPA | `ios install` | `installation_proxy::install`（需先 AFC 上传）或 `installcoordination_proxy` | 已有 | — |
| 列出 App（过滤 hidden 系统服务） | `ios apps --system` + 过滤 `SBAppTags` 含 `hidden` | `installation_proxy::get_apps`（取 `SBAppTags`） | 已有 + 需自写（过滤） | 必须保留 hidden 过滤，否则「设置」会匹配到 5 个后台服务 |

## 2. MCP 工具（toolkit 的 13 个 + 2 个扩展）

| 工具 | toolkit 现在靠 | D 方案实现 | 状态 | 风险 / 待验证 |
|---|---|---|---|---|
| `iphone_status` | WDA `/status`、`/wda/activeAppInfo`、`/wda/locked` | 守护进程状态机快照 + WDA 查询 | 需自写 | — |
| `iphone_screenshot`（含区域放大） | WDA `/screenshot`，退回 pmd3 `screen-capture` | WDA `screenshot`；退回 `core_device::screencaptureservices::take_screenshot(Png)`；缩放裁剪用 `image` crate | 已有 + 需自写（缩放） | 截图走隧道时的耗时待测 |
| `iphone_list_elements` | WDA `/source?format=xml&excluded_attributes=visible,...` + Python 解析 | `ictl-wda` 调 source；解析用 `quick-xml`；可见性按坐标判断、去掉全屏容器、去重 | 需自写 | idevice `WdaClient::source` 不支持 `excluded_attributes` 参数，需自己发请求（完整树 7～8 秒，排除后 0.3 秒） |
| `iphone_tap` | WDA `/wda/tap`，退回 HID | WDA `tap`；退回 `UniversalHidServiceClient::tap` | 已有 | HID 需要显示媒体流同时存在 |
| `iphone_tap_element` | 解析元素树按文字匹配后点中心 | 同左，纯逻辑 | 需自写 | — |
| `iphone_long_press` | WDA `/wda/touchAndHold` | WDA `touch_and_hold` | 已有 | — |
| `iphone_swipe` | WDA W3C `/actions`，退回 HID `drag` | W3C actions（需自己构造）；退回 `UniversalHidServiceClient::drag` | 已有 + 需自写（actions 构造） | — |
| `iphone_type_text`（含中文） | WDA `/wda/keys`；退回 HID `type`（仅 ASCII） | WDA `send_keys`；退回 `MainKeyboardService`（HID 键盘，仅能按键码） | 已有 | HID 路线不能输中文，保持现状的提示 |
| `iphone_press_button` | WDA `/wda/pressButton`，锁屏键走 HID | WDA `press_button`；锁屏 `/wda/lock`；退回 `IndigoHidClient::send_button` | 已有 | indigo 与 universalhidservice 在 iOS 27 上各自能用哪些待验证 |
| `iphone_list_apps` | `ios apps` | `installation_proxy::get_apps` | 已有 | — |
| `iphone_launch_app` | WDA `/wda/apps/launch` + 等前台 1 秒 | WDA `launch_app` + 轮询 activeAppInfo；退回 `core_device::app_service::launch_application` | 已有 + 需自写（等前台） | 冷启动立即点击会被吞，必须保留等待 |
| `iphone_terminate_app` | WDA `/wda/apps/terminate` | WDA `terminate_app` | 已有 | — |
| `iphone_open_url` | WDA `/url` | WDA 会话 `/url`（idevice 未封装，自己发） | 需自写 | — |
| `iphone_wait_for`（新增） | 无 | 守护进程轮询元素树，出现即返回 | 已实现 | 只认元素树里的文字，游戏等没有元素的界面要截图 |
| `iphone_batch`（新增） | 无 | MCP 层按顺序调用守护进程，出错或按名字没点到即停；步骤先全部校验再执行；`reuse_elements` 时按名字点击复用守护进程缓存的元素树（10 秒内，任何其他操作都会让缓存作废） | 已实现 | 步骤间不看界面，只适合结果确定的连续操作；`reuse_elements` 只用于不改变布局的连按，误用时下一步多半找不到元素而停下 |
| 操作类工具的 `then` 参数（新增） | 无 | 操作后 `Settle`，再附元素列表或截图（`then_region` 只返回某块区域的放大图）。有元素树的界面看连续两次元素树是否一样；元素少于 12 个的界面（游戏、地图）改看画面：48×104 灰度缩略图连续两帧平均差 < 3 算静止。最多等 `then_settle` 秒（默认 1.5，最多 10） | 已实现 | 每次多 0.5～1.3 秒等稳定，换一次往返；画面一直有小动画的界面会等满上限 |

## 3. 后台能力

| 能力 | toolkit 现在靠 | D 方案实现 | 状态 | 风险 / 待验证 |
|---|---|---|---|---|
| 启动 WDA | `ios runwda --bundleid ... --xctestconfig=WebDriverAgentRunner.xctest` | `XCUITestService::run_until_wda_ready()`，`TestConfig::from_installation_proxy(runner_bundle_id)`；返回 `WdaRunHandle`（`abort`、`wait`、`status`） | 已有 | iOS 27.0.1 上能否启动待验证；自动化密码框的错误文本要能识别 |
| WDA 访问方式 | `pymobiledevice3 usbmux forward` 绑 127.0.0.1:8100 | 自己的客户端（`crates/ictl-wda/src/http.rs` + `client.rs`）经 usbmuxd 直连设备 8100 端口、自己发 HTTP，本机不监听；不用 idevice 的 `WdaClient` | 需自写 | idevice 的 `WdaClient` 自述是"bootstrap"级，且不支持 `excluded_attributes`、appium settings、W3C actions |
| 守护与自恢复 | `supervisor.sh` | `ictl-daemon` 状态机 | 需自写 | 见 architecture §3 |
| Apple ID 登录、注册设备、描述文件 | plumesign 命令行 | `plume_core::auth::account`、`developer::qh::{devices, app_ids, profile, certs}`；anisette 用 `omnisette` | 已有 | 默认 anisette 服务是第三方（jkcoxson），需可配置；会话持久化格式自己定 |
| 签名 WDA（含 `.xctest` 插件） | go-ios `ios sign app`（因为 plumesign 漏签 xctest） | `ictl-sign`：bundle 逻辑改写自 `plume_utils`（含修复），签名流程自己写；底层 `apple-codesign` | **需修** | 根因见下文 §4 |
| 7 天自动重签 | `resign-wda.sh` + systemd 定时器 | 守护进程内部调度 | 需自写 | — |
| 通知（Bark / Telegram） | `notify.sh`（curl） | `reqwest` | 需自写 | — |
| 锁屏自动解锁 | `iphone_mcp.py` `_ensure_unlocked` | `ictl-unlock` | 需自写 | 见 §5 |
| 诊断（doctor） | 无（靠人工排查） | `apps/iphone-control doctor` 逐层检查 | 需自写 | — |

## 4. `.xctest` 漏签：根因和修复

**根因**（读 Impactor `crates/plume_utils/src/bundle.rs` 与 `signer.rs` 确认）：

1. `collect_embeded_bundles_from_dir` 会把任何带扩展名的目录当成 bundle，所以 `PlugIns/WebDriverAgentRunner.xctest` 能被找到。
2. 但 `BundleType::from_extension` 只认识 `app`、`appex`、`framework`、`dylib`，其余（包括 `xctest`）都是 `BundleType::Unknown`。
3. `Signer::sign_single_bundle` 开头 `if bundle_type == Unknown { return Ok(()) }`，直接跳过。
4. 结果：xctest 内部的 `WebDriverAgentLib.framework` 被签了（Framework 类型），xctest 本身的二进制没签、没有 `_CodeSignature`，外层 app 签名时把它当普通文件封存。iOS 加载测试包时报 `XCTestErrorDomain` 103。

**修复**（几行）：

```rust
pub enum BundleType { App, AppExtension, XCTest, Framework, Dylib, Unknown }
// from_extension:
"xctest" => Some(BundleType::XCTest),
// should_be_signed: XCTest 返回 true
// should_have_entitlements: XCTest 返回 false
```

依据：go-ios 签好、能正常启动的包里，xctest 二进制带开发证书签名、**不带** entitlements（`get-task-allow` 只在 runner app 主程序上）。

签名顺序不用改：`collect_bundles_sorted` 已经按路径深度从深到浅排序，xctest 会先于外层 app 签名。修好后应向 Impactor 提 PR。

## 5. 自动解锁要移植的规则

全部来自 toolkit 调试结论，`ictl-unlock` 必须原样保留：

1. 先读 `/wda/locked`，没锁就直接返回。
2. 唤醒流程固定为：按 Home → 等 1.0 秒 → 从底部上滑 → 等 1.5 秒。不要依赖"元素树里有没有键盘"，键盘元素在不显示时也常驻树中；`displayed` 接口在锁屏上同样不可靠。
3. 读 `XCUIElementTypeSecureTextField` 的值（形如「共6位数，已输入N位」），提取两个整数；总位数和配置不一致就一位都不输。
4. 每一位用**单独一个** W3C 手势（按下 80ms）点击，再核对已输入位数正好 +1；对不上立即停止。输入不完整不算输错。合并成一个手势会漏键；`/wda/tap` 在锁屏上每次 2.9 秒，太慢。
5. 解锁期间把 appium settings `waitForIdleTimeout` 压到 1（不能是 0，否则唤醒失败）、`animationCoolOffTimeout` 压到 0.2，结束后恢复。锁屏界面的 Face ID 动画会让 WDA 每个请求等满 10 秒，期间密码键盘会自动收起。
6. 第一位没生效时可以重来（最多 3 轮）；整串输完仍锁着才算真正输错一次：写停用标记、推送通知、之后拒绝自动解锁直到人工清除。
7. 只支持纯数字密码。重启后首次解锁（BFU）永远需要人工。
