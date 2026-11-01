//! iphone-control：唯一的可执行程序。守护进程、MCP 服务和运维命令都在这里。
//!
//! 运维命令（status、doctor、resign、unlock-reset）经控制接口问守护进程；
//! setup、login、passcode 需要交互，直接在本进程里做。

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use ictl_daemon::protocol::{self, Request, Response};

mod debug;

#[derive(Parser)]
#[command(name = "iphone-control", version, about = "在 Linux 上控制 iPhone，并提供 MCP 工具")]
struct Cli {
    /// 数据根目录（账号、描述文件、日志等）。默认：root 是 /var/lib/iphone-control，普通用户是 ~/.local/share/iphone-control
    #[arg(long, env = "ICTL_DATA_DIR", default_value_os_t = protocol::default_data_dir())]
    data_dir: PathBuf,
    /// 守护进程控制接口。默认：root 是 /run/iphone-control/ctl.sock，普通用户是 $XDG_RUNTIME_DIR/iphone-control/ctl.sock
    #[arg(long, env = "ICTL_SOCKET", default_value_os_t = protocol::default_socket())]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 常驻守护进程（容器入口；从源码运行时由 systemd 或手动启动）
    Daemon,
    /// MCP stdio 服务（由 agent 拉起）
    Mcp,
    /// 打印把 MCP 注册到 Claude Code 的命令：在容器里打印 docker exec 的写法，否则直接调用本程序
    McpConfig {
        /// 容器名；给了就按容器部署打印
        #[arg(long)]
        container: Option<String>,
    },
    /// 守护进程和设备状态、最近的日志
    Status,
    /// 逐层诊断，每项失败都给出修复办法
    Doctor,
    /// 首次安装向导：配对 → 开发者模式 → 登录 Apple ID → 签名安装 WDA（可重复运行）
    Setup,
    /// 登录 Apple ID（密码和双重认证验证码只在这里输入）
    Login {
        /// Apple ID 邮箱；不给就交互输入
        email: Option<String>,
    },
    /// 立即签名并安装 WDA
    Sign,
    /// 检查描述文件，快过期就重签；--force 立即重签
    Resign {
        #[arg(long)]
        force: bool,
    },
    /// 设置/删除锁屏密码（自动解锁，默认关闭）
    Passcode {
        #[command(subcommand)]
        action: PasscodeAction,
    },
    /// 自动解锁因输错被停用后，核对密码再恢复
    UnlockReset,
    /// 显式把 WDA 桥接到 127.0.0.1 供外部工具使用（默认不监听任何端口）
    WdaBridge {
        #[arg(long, default_value_t = 8100)]
        port: u16,
    },
    /// 真机诊断（原第 0 步 spike）。`wda`/`e2e`/`hid` 会和守护进程抢 XCTest、HID，先停掉守护进程
    #[command(hide = true)]
    Debug(debug::DebugArgs),
    /// 按已配置的渠道发测试通知：每类事件各一条（标题带「[测试]」），或只发指定的一类
    NotifyTest {
        /// resign / session / pin / pin-recovered / unlock / offline / online；不给就全发
        event: Option<String>,
    },
}

#[derive(Subcommand)]
enum PasscodeAction {
    Set,
    Clear,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 依赖树里 rustls 的 ring 和 aws-lc-rs 都开着，不装默认 provider 的话第一个 HTTPS 请求就 panic
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    let quiet = matches!(cli.command, Command::Mcp);
    // ICTL_LOG 留空（.env 里常见的 `ICTL_LOG=`）当作没设；原来空值会生成一个什么都不放行的过滤器
    let filter =
        env_nonempty("ICTL_LOG").and_then(|v| tracing_subscriber::EnvFilter::try_new(v).ok()).unwrap_or_else(|| {
            tracing_subscriber::EnvFilter::new(if quiet { "warn" } else { "info,idevice=warn,apple_codesign=warn" })
        });
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(filter)
        .init();

    match cli.command {
        Command::Daemon => {
            let mut cfg = ictl_daemon::Config::from_env(&cli.data_dir);
            cfg.socket = cli.socket;
            ictl_daemon::run(cfg).await
        }
        Command::Mcp => ictl_mcp::serve_stdio(cli.socket).await,
        Command::McpConfig { container } => {
            println!("{}", mcp_config(container, &cli.socket)?);
            Ok(())
        }
        Command::Status => status(&cli.socket).await,
        Command::Doctor => print_text(&cli.socket, Request::Doctor).await,
        Command::Setup => setup(&cli).await,
        Command::Login { email } => login(&cli.data_dir, email).await,
        Command::Sign => print_text(&cli.socket, Request::Resign { force: true }).await,
        Command::Resign { force } => print_text(&cli.socket, Request::Resign { force }).await,
        Command::Passcode { action } => passcode(&cli.data_dir, action),
        Command::UnlockReset => print_text(&cli.socket, Request::UnlockReset).await,
        Command::WdaBridge { port } => wda_bridge(port).await,
        Command::Debug(args) => debug::run(env_nonempty("IPHONE_UDID").as_deref(), args).await,
        Command::NotifyTest { event } => notify_test(&cli.data_dir, event.as_deref()).await,
    }
}

async fn notify_test(data_dir: &Path, only: Option<&str>) -> anyhow::Result<()> {
    use ictl_daemon::notify::{Event, send};
    let cfg = ictl_daemon::Config::from_env(data_dir).notify;
    let channels =
        usize::from(cfg.bark_key.is_some()) + usize::from(cfg.tg_bot_token.is_some() && cfg.tg_chat_id.is_some());
    if channels == 0 {
        bail!(
            "没有配置通知渠道：填 BARK_KEY，或 TG_BOT_TOKEN + TG_CHAT_ID（容器部署在 .env 里填、重建容器；\
             从源码运行在守护进程的环境文件里填、重启守护进程）"
        );
    }
    let events: Vec<Event> =
        Event::samples().into_iter().filter(|e| only.is_none_or(|n| e.sample_name() == n)).collect();
    if events.is_empty() {
        let names: Vec<&str> = Event::samples().iter().map(|e| e.sample_name()).collect();
        bail!("没有这类事件，可选：{}", names.join(" / "));
    }
    let mut failed = 0;
    for e in &events {
        let (title, body) = e.title_body();
        let ok = send(&cfg, &format!("[测试] {title}"), &body).await;
        println!(
            "{} {:<14} {title}（{ok}/{channels} 个渠道成功）",
            if ok == channels { "✅" } else { "❌" },
            e.sample_name()
        );
        failed += usize::from(ok < channels);
        // Bark 同组消息太密会被手机合并，隔一下
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if failed > 0 {
        bail!("{failed} 条没有全部发出去，详见上面的 WARN 日志");
    }
    Ok(())
}

/// 容器部署：MCP 进程由 `docker exec -i` 在容器里拉起。从源码运行：直接调用本程序，
/// 显式带上 socket，免得 Claude Code 的环境里没有 XDG_RUNTIME_DIR 时算出别的位置
fn mcp_config(container: Option<String>, socket: &Path) -> anyhow::Result<String> {
    let in_container = Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists();
    let container = container
        .or_else(|| in_container.then(|| env_nonempty("CONTAINER_NAME").unwrap_or_else(|| "iphone-control".into())));
    Ok(match container {
        Some(c) => format!("claude mcp add iphone -s user -- docker exec -i {c} iphone-control mcp"),
        None => {
            let exe = std::env::current_exe().context("找不到本程序的路径")?;
            format!(
                "claude mcp add iphone -s user -- {} --socket {} mcp",
                shell_quote(&exe.to_string_lossy()),
                shell_quote(&socket.to_string_lossy())
            )
        }
    })
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:@%".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// 读环境变量，去掉首尾空白，空值当作没设（.env 里常留着 `KEY=`）
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

async fn ask(socket: &Path, req: Request) -> anyhow::Result<Response> {
    // 签名要等装完，不设时限；其余命令守护进程忙也最多等 3 分钟
    let timeout = match req {
        Request::Resign { .. } => None,
        _ => Some(Duration::from_secs(180)),
    };
    protocol::call(socket, &req, timeout).await.with_context(|| {
        format!(
            "连不上守护进程（{}）：先启动 iphone-control daemon（容器里是入口进程）；\
             守护进程以别的用户运行时，用 --socket 或 ICTL_SOCKET 指到它的控制接口",
            socket.display()
        )
    })
}

async fn print_text(socket: &Path, req: Request) -> anyhow::Result<()> {
    match ask(socket, req).await? {
        Response::Text { text } => {
            println!("{text}");
            Ok(())
        }
        Response::Error { message } => bail!("{message}"),
        Response::Image { caption, .. } => {
            println!("{caption}");
            Ok(())
        }
    }
}

async fn snapshot(socket: &Path) -> anyhow::Result<ictl_daemon::Snapshot> {
    match ask(socket, Request::Daemon).await? {
        Response::Text { text } => Ok(serde_json::from_str(&text)?),
        Response::Error { message } => bail!("{message}"),
        Response::Image { .. } => bail!("守护进程返回了意外的响应"),
    }
}

async fn status(socket: &Path) -> anyhow::Result<()> {
    let s = snapshot(socket).await?;
    println!("状态      {}", s.state.describe());
    if let Some(d) = &s.device {
        println!("手机      {} {} iOS {}", d.udid, d.product_type, d.product_version);
    }
    let p = &s.persisted;
    println!("WDA       {}", p.wda_bundle.as_deref().unwrap_or("未签名"));
    if let Some(left) = p.profile_left() {
        println!("描述文件  还剩 {} 小时", left.as_secs() / 3600);
    }
    if let Some(r) = &p.last_resign {
        println!("上次重签  {r}");
    }
    let unlock = match (s.auto_unlock, s.unlock_disabled) {
        (_, true) => "已停用（之前输错过，iphone-control unlock-reset）",
        (true, false) => "已启用",
        (false, false) => "未启用",
    };
    println!("自动解锁  {unlock}");
    println!("通知      {}", if s.notify_configured { "已配置" } else { "未配置" });
    println!("\n最近日志：");
    for l in s.log_tail {
        println!("  {l}");
    }
    Ok(())
}

fn read_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    if std::io::stdin().lock().read_line(&mut s)? == 0 {
        bail!("读不到输入（标准输入已关闭）：在终端里运行；容器部署用 docker exec -it");
    }
    Ok(s.trim().to_owned())
}

/// 不回显地读一行（用 stty 关回显，不引入额外依赖）
fn read_secret(prompt: &str) -> anyhow::Result<String> {
    let stty = |arg: &str| {
        std::process::Command::new("stty")
            .arg(arg)
            .stdin(
                std::fs::File::open("/dev/tty").map(std::process::Stdio::from).unwrap_or(std::process::Stdio::null()),
            )
            .status()
    };
    let echo_off = stty("-echo").map(|s| s.success()).unwrap_or(false);
    let r = read_line(prompt);
    if echo_off {
        let _ = stty("echo");
        println!();
    }
    r
}

/// 账号目录和 anisette 地址：与守护进程同一份配置（路径、环境变量的读法都在 Config 里）
fn account_dir(data_dir: &Path) -> ictl_sign::AccountDir {
    let cfg = ictl_daemon::Config::from_env(data_dir);
    ictl_sign::AccountDir { dir: cfg.account_dir(), anisette_url: cfg.anisette_url }
}

async fn login(data_dir: &Path, email: Option<String>) -> anyhow::Result<()> {
    println!("建议用一个专门的免费 Apple ID，不要用主账号。");
    println!(
        "密码和验证码由本程序直接发给苹果；登录时会请求 anisette 服务生成设备标识（默认第三方，可用 ICTL_ANISETTE_URL 改成自建）。"
    );
    let email = match email {
        Some(e) => e,
        None => read_line("Apple ID 邮箱: ")?,
    };
    if email.is_empty() {
        bail!("没有输入 Apple ID 邮箱");
    }
    let password = read_secret("密码(输入不显示): ")?;
    if password.is_empty() {
        bail!("没有输入密码");
    }
    let two_factor = |req: ictl_sign::TwoFactorRequest| -> Result<ictl_sign::TwoFactorAction, String> {
        let sms = !req.trusted_phone_numbers.is_empty();
        let prompt = match req.method {
            ictl_sign::TwoFactorMethod::Sms => "短信验证码: ",
            ictl_sign::TwoFactorMethod::Device if sms => "设备上收到的验证码(输入 sms 改为短信接收): ",
            ictl_sign::TwoFactorMethod::Device => "设备上收到的验证码: ",
        };
        let code = read_line(prompt).map_err(|e| e.to_string())?;
        if sms && code.eq_ignore_ascii_case("sms") {
            return Ok(ictl_sign::TwoFactorAction::SendSms(req.trusted_phone_numbers[0].id));
        }
        Ok(ictl_sign::TwoFactorAction::SubmitCode(code))
    };
    account_dir(data_dir).login(&email, &password, two_factor).await?;
    println!("登录成功，会话已保存到 {}", account_dir(data_dir).dir.display());
    Ok(())
}

fn passcode(data_dir: &Path, action: PasscodeAction) -> anyhow::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = ictl_daemon::Config::from_env(data_dir).passcode_file();
    match action {
        PasscodeAction::Clear => {
            let _ = std::fs::remove_file(&path);
            println!("已删除锁屏密码，自动解锁关闭。");
        }
        PasscodeAction::Set => {
            println!("自动解锁：锁屏后，MCP 操作前会用这个密码解锁手机。");
            println!("风险：能登录这台机器、或能调用这个 MCP 的人，都能解锁你的手机。");
            println!("保护：每按一位都核对；整串输完仍锁着就停用自动解锁并推送通知，不会累计输错。只支持纯数字。");
            let p = read_secret("锁屏密码(输入不显示): ")?;
            if p.len() < 4 || !p.bytes().all(|b| b.is_ascii_digit()) {
                bail!("只支持 4 位以上的纯数字密码");
            }
            let dir = path.parent().context("密码文件路径没有上级目录")?;
            std::fs::create_dir_all(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path)?;
            // mode() 只在新建时生效：已有文件也收紧
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.write_all(p.as_bytes())?;
            println!("已保存。别忘了在手机上把「自动锁定」从「永不」改回较短时间。");
        }
    }
    Ok(())
}

/// 每隔 `every` 检查一次，直到 `check` 返回 true 或超时
async fn wait_until<F, Fut>(timeout: Duration, every: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check().await {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(every).await;
    }
}

async fn setup(cli: &Cli) -> anyhow::Result<()> {
    use ictl_device::{Device, DeviceError};
    let socket = &cli.socket;
    snapshot(socket).await.context("setup 需要守护进程在运行")?;

    println!("[1/6] 等待手机连接");
    let found = wait_until(Duration::from_secs(600), Duration::from_secs(5), || async {
        ictl_device::list_devices().await.map(|d| !d.is_empty()).unwrap_or(false)
    })
    .await;
    if !found {
        bail!("10 分钟内没检测到手机：用数据线接好并解锁手机");
    }
    let udid = env_nonempty("IPHONE_UDID");
    let device = Device::find(udid.as_deref()).await?;
    println!("  手机 {}", device.udid);

    println!("[2/6] 配对");
    if device.paired().await? {
        println!("  已配对");
    } else {
        println!("  手机上会弹出「要信任此电脑吗?」:点「信任」,再输入锁屏密码。");
        let ok = wait_until(Duration::from_secs(300), Duration::from_secs(5), || async {
            match device.pair("iphone-control").await {
                Ok(()) => true,
                Err(DeviceError::PairingPending | DeviceError::PasswordProtected) => false,
                Err(e) => {
                    eprintln!("  {e}");
                    false
                }
            }
        })
        .await;
        if !ok {
            bail!("5 分钟内没完成配对");
        }
        println!("  配对成功");
    }

    println!("[3/6] 开发者模式");
    if device.developer_mode_enabled().await.unwrap_or(false) {
        println!("  已打开");
    } else {
        let _ = device.reveal_developer_mode().await;
        println!("  请在手机上:设置 → 隐私与安全性 → 开发者模式 → 打开 → 重新启动;");
        println!("  重启后解锁,在弹窗里点「打开」并输入密码。手机重启期间连接会断开,会自动恢复。");
        let ok = wait_until(Duration::from_secs(900), Duration::from_secs(10), || async {
            match Device::find(udid.as_deref()).await {
                Ok(d) => d.developer_mode_enabled().await.unwrap_or(false),
                Err(_) => false,
            }
        })
        .await;
        if !ok {
            bail!("15 分钟内没检测到开发者模式打开");
        }
        println!("  已打开");
    }

    println!("[4/6] 登录 Apple ID");
    match account_dir(&cli.data_dir).selected_email().await {
        Ok(e) => println!("  已登录 {e}"),
        Err(_) => login(&cli.data_dir, None).await?,
    }

    println!("[5/6] 注册设备、签名 WDA 并安装到手机（手机要保持解锁）");
    print_text(socket, Request::Resign { force: true }).await?;

    println!("[6/6] 等待 WDA 启动");
    println!("  首次安装:在手机 设置 → 通用 → VPN 与设备管理 里找到你的 Apple ID,点「信任」(需要联网)。");
    println!("  如果弹出「为 XCTest 输入 iPhone 密码」,输入锁屏密码。");
    let ready = wait_until(Duration::from_secs(600), Duration::from_secs(10), || async {
        snapshot(socket).await.map(|s| s.state == ictl_daemon::machine::DeviceState::Ready).unwrap_or(false)
    })
    .await;
    println!("  {}", if ready { "WDA 就绪" } else { "10 分钟内 WDA 没就绪,运行 iphone-control doctor 看看卡在哪" });

    println!("\n可选设置");
    println!("  通知:填 TG_* 或 BARK_*(容器部署在 .env 里,然后重建容器;从源码运行在守护进程的环境文件里,然后重启它)");
    println!("  自动解锁:iphone-control passcode set");
    println!("  注册到 Claude Code:iphone-control mcp-config");
    println!();
    print_text(socket, Request::Doctor).await
}

/// 显式开启：127.0.0.1:port → 手机上的 WDA 8100。只绑本机回环地址。
/// 容器用 host 网络，所以这就是宿主机的回环地址：本机的其他用户、浏览器里的网页都能访问到，而 WDA 没有认证。
/// 浏览器那条路（DNS rebinding）靠检查 Host 头挡住；本机其他用户挡不住，用完就停。
async fn wda_bridge(port: u16) -> anyhow::Result<()> {
    let udid = env_nonempty("IPHONE_UDID");
    let device = ictl_device::Device::find(udid.as_deref()).await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("WDA 桥接 127.0.0.1:{port} → 手机 {} 的 8100 端口（Ctrl-C 停止）", device.udid);
    println!("注意：本机所有用户都能经这个端口控制手机（WDA 没有认证），用完就停掉。");
    loop {
        let (client, _) = listener.accept().await?;
        let device = device.clone();
        tokio::spawn(async move {
            if let Err(e) = bridge_conn(client, &device).await {
                tracing::warn!("桥接连接出错：{e}");
            }
        });
    }
}

/// 先读到第一个请求的头部，Host 必须是本机回环地址才转发：恶意网页借 DNS rebinding 发到 127.0.0.1 的请求，
/// Host 头是它自己的域名
async fn bridge_conn(mut client: tokio::net::TcpStream, device: &ictl_device::Device) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > 64 * 1024 {
            bail!("请求头太长");
        }
        let n = client.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    if !host_is_loopback(&head) {
        client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
        return Ok(());
    }
    let mut remote = device.connect_port(8100).await?;
    remote.write_all(&head).await?;
    tokio::io::copy_bidirectional(&mut client, &mut remote).await?;
    Ok(())
}

/// HTTP 请求头里的 Host 是不是 localhost / 127.0.0.1 / [::1]（端口不限）
fn host_is_loopback(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    let Some(host) = text.split("\r\n").skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("host").then(|| v.trim().to_ascii_lowercase())
    }) else {
        return false;
    };
    let name = match host.strip_prefix('[') {
        Some(rest) => format!("[{}]", rest.split(']').next().unwrap_or_default()),
        None => host.split(':').next().unwrap_or_default().to_owned(),
    };
    matches!(name.as_str(), "localhost" | "127.0.0.1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::host_is_loopback;

    #[test]
    fn bridge_only_accepts_loopback_host() {
        assert!(host_is_loopback(b"GET /status HTTP/1.1\r\nHost: 127.0.0.1:8100\r\n\r\n"));
        assert!(host_is_loopback(b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n"));
        assert!(host_is_loopback(b"GET / HTTP/1.1\r\nHost: [::1]:8100\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\nHost: evil.example:8100\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\nHost: localhost.evil.example\r\n\r\n"));
        assert!(!host_is_loopback(b"GET / HTTP/1.1\r\n\r\n"));
    }
}
