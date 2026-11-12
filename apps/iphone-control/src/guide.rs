//! The first-run guide: `iphone-control` with no arguments.
//!
//! Until setup has been completed once it walks the user through everything, one step at a time, in a terminal:
//! language, proxy, the background service, the phone (pairing, Developer Mode, Apple ID, WebDriverAgent), the
//! optional auto-unlock passcode and notifications, and registering the MCP server with Claude Code and Codex.
//! Steps that are already done say so and move on, so it is safe to run again. Settings go to
//! `~/.config/iphone-control/env` ([`crate::envfile`]); the daemon is restarted when they change.
//!
//! When it is done it leaves a marker ([`marker`]); from then on `iphone-control` only makes sure the daemon runs
//! and shows the status. `iphone-control setup --reconfigure` redoes all of it or one part.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::bail;
use ictl_l10n::Lang;

use crate::envfile::{self, EnvFile};
use crate::{Cli, service};

mod terminal;
use terminal::{ask_yes, choose};

/// Cancellation is a normal end to the guide, not a failed setup. Completed
/// steps may already have written settings or changed the system; do not undo them.
fn finish(result: anyhow::Result<()>) -> anyhow::Result<()> {
    match result {
        Err(error) if error.is::<terminal::Cancelled>() => {
            println!(
                "\n{}",
                ictl_l10n::tr!(
                    "已取消当前向导。已保存的设置和已完成的步骤会保留；当前未保存的输入会丢弃。重新运行可继续设置。",
                    "Setup cancelled. Saved settings and completed steps are kept; unsaved input is discarded. Run setup again to continue."
                )
            );
            Ok(())
        }
        other => other,
    }
}

fn marker() -> PathBuf {
    envfile::config_dir().join("setup-done")
}

pub fn done() -> bool {
    marker().is_file()
}

fn mark_done() -> anyhow::Result<()> {
    std::fs::create_dir_all(envfile::config_dir())?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    std::fs::write(marker(), format!("version={}\ncompleted_at={now}\n", env!("CARGO_PKG_VERSION")))?;
    Ok(())
}

fn heading(zh: &str, en: &str) {
    println!("\n━━ {} ━━", if ictl_l10n::is_en() { en } else { zh });
}

fn need_terminal() -> anyhow::Result<()> {
    // Keep setup's existing safety boundary: piped answers must not start
    // services or change a device. TERM=dumb still works with line prompts.
    if !std::io::stdin().is_terminal() {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "设置要在终端里交互完成：直接在终端里运行 iphone-control",
                "Setup is interactive: run iphone-control in a terminal"
            )
        );
    }
    Ok(())
}

/// `iphone-control` with no arguments
pub async fn first_run(cli: &Cli) -> anyhow::Result<()> {
    finish(first_run_inner(cli).await)
}

async fn first_run_inner(cli: &Cli) -> anyhow::Result<()> {
    if done() {
        return already_set_up(cli).await;
    }
    need_terminal()?;
    println!(
        "{}",
        ictl_l10n::tr!(
            "欢迎使用 iphone-control。接下来一步步完成设置，已经做好的步骤会自动跳过，中途退出后再运行 iphone-control 会接着做。",
            "Welcome to iphone-control. Setup goes step by step; steps already done are skipped, and if you stop halfway, running iphone-control again picks up where you left."
        )
    );
    full(cli).await
}

/// `iphone-control setup [--reconfigure]`
pub async fn setup(cli: &Cli, reconfigure: bool) -> anyhow::Result<()> {
    finish(setup_inner(cli, reconfigure).await)
}

async fn setup_inner(cli: &Cli, reconfigure: bool) -> anyhow::Result<()> {
    need_terminal()?;
    if !reconfigure {
        return full(cli).await;
    }
    let items = [
        ictl_l10n::tr!("全部重新走一遍", "All of it"),
        ictl_l10n::tr!("语言", "Language"),
        ictl_l10n::tr!("代理", "Proxy"),
        ictl_l10n::tr!("后台服务", "Background service"),
        ictl_l10n::tr!("Apple ID（重新登录并重签 WDA）", "Apple ID (sign in again and re-sign WDA)"),
        ictl_l10n::tr!("手机（配对、开发者模式、安装 WDA）", "Phone (pairing, Developer Mode, WDA)"),
        ictl_l10n::tr!("自动解锁密码", "Auto-unlock passcode"),
        ictl_l10n::tr!("通知", "Notifications"),
        ictl_l10n::tr!("注册 MCP（Claude Code、Codex）", "MCP registration (Claude Code, Codex)"),
    ];
    let pick = choose(ictl_l10n::tr!("要重新设置哪一项？", "What do you want to set up again?"), &items, 0)?;
    let mut env = EnvFile::open(&envfile::path())?;
    match pick {
        0 => {
            let _ = std::fs::remove_file(marker());
            return full_with(cli, true).await;
        }
        1 => {
            if language(&mut env, true)? {
                env.save()?;
                daemon(cli, true).await?;
            }
        }
        2 => {
            if proxy(&mut env, true)? {
                env.save()?;
                daemon(cli, true).await?;
            }
        }
        3 => reinstall_service(cli).await?,
        4 => {
            daemon(cli, false).await?;
            crate::login(&cli.data_dir, None).await?;
            println!("{}", ictl_l10n::tr!("用新的账号重签 WDA…", "Re-signing WDA with the account…"));
            crate::print_text(&cli.socket, ictl_daemon::protocol::Request::Resign { force: true }).await?;
        }
        5 => {
            daemon(cli, false).await?;
            crate::device_setup(cli).await?;
        }
        6 => {
            daemon(cli, false).await?;
            passcode(cli, true).await?;
        }
        7 => {
            notify_step(cli, &mut env, true).await?;
        }
        _ => register_mcp(cli, true)?,
    }
    println!("\n{}", ictl_l10n::tr!("完成。", "Done."));
    Ok(())
}

async fn full(cli: &Cli) -> anyhow::Result<()> {
    full_with(cli, false).await
}

/// Every step; `force` asks again about the optional ones that are already set
async fn full_with(cli: &Cli, force: bool) -> anyhow::Result<()> {
    let mut env = EnvFile::open(&envfile::path())?;

    heading("语言 / Language", "Language / 语言");
    // Save these together immediately before applying them to the daemon.
    // Cancelling at the proxy menu leaves the previous settings intact.
    let mut changed = language(&mut env, force)?;
    heading("代理", "Proxy");
    changed |= proxy(&mut env, force)?;
    if changed {
        env.save()?;
    }

    heading("后台服务", "Background service");
    daemon(cli, changed).await?;

    heading("手机", "Phone");
    crate::device_setup(cli).await?;

    heading("自动解锁（可选）", "Auto-unlock (optional)");
    passcode(cli, force).await?;

    heading("通知（可选）", "Notifications (optional)");
    notify_step(cli, &mut env, force).await?;

    heading("注册 MCP", "MCP registration");
    register_mcp(cli, force)?;

    mark_done()?;
    heading("完成", "All set");
    println!(
        "{}",
        ictl_l10n::trf!(
            "设置保存在 {}，以后直接运行 iphone-control 就能查看状态，不用再设置。\n\
             - 在 AI 客户端里使用：开一个新的 Claude Code 或 Codex 会话，试着说「打开时钟，切到秒表并启动」\n\
             - 在浏览器里看画面、操作手机、交给本机的 AI 执行任务：iphone-control viewer\n\
             - 改设置：iphone-control setup --reconfigure",
            "Settings are in {}. From now on, running iphone-control just shows the status; no more setup.\n\
             - In an AI client: open a new Claude Code or Codex session and ask, for example, “Open Clock, switch to the stopwatch and start it”\n\
             - In a browser, to watch and control the phone and hand tasks to a local AI: iphone-control viewer\n\
             - To change settings: iphone-control setup --reconfigure",
            envfile::path().display()
        )
    );
    Ok(())
}

/// Already set up: make sure the daemon runs, then show where things stand
async fn already_set_up(cli: &Cli) -> anyhow::Result<()> {
    if crate::snapshot(&cli.socket).await.is_err()
        && let Some(s) = service::find()
    {
        println!("{}", ictl_l10n::tr!("守护进程没在运行，正在启动…", "The daemon isn't running, starting it…"));
        service::start(&s)?;
        wait_daemon(&cli.socket, Duration::from_secs(60)).await;
    }
    crate::status(&cli.socket).await?;
    println!(
        "\n{}",
        ictl_l10n::tr!(
            "浏览器控制台：iphone-control viewer · 排查问题：iphone-control doctor · 改设置：iphone-control setup --reconfigure",
            "Control page: iphone-control viewer · Troubleshoot: iphone-control doctor · Change settings: iphone-control setup --reconfigure"
        )
    );
    Ok(())
}

/// Asks for the language and switches this run to it. Returns whether the file changed
fn language(env: &mut EnvFile, force: bool) -> anyhow::Result<bool> {
    let current = env.get(ictl_l10n::ENV).and_then(|v| ictl_l10n::parse(&v));
    if let (Some(l), false) = (current, force) {
        ictl_l10n::set(l);
        println!("{}", ictl_l10n::tr!("  中文（已设置）", "  English (already set)"));
        return Ok(false);
    }
    let default = usize::from(current.unwrap_or_else(ictl_l10n::own_lang) == Lang::En);
    let l = if choose("界面语言 / Language", &["中文", "English"], default)? == 1 { Lang::En } else { Lang::Zh };
    ictl_l10n::set(l);
    env.set(ictl_l10n::ENV, Some(l.code()))
}

fn proxy(env: &mut EnvFile, force: bool) -> anyhow::Result<bool> {
    let current = env.get("HTTPS_PROXY").or_else(|| env.get("HTTP_PROXY"));
    if !force && let Some(p) = &current {
        println!("{}", ictl_l10n::trf!("  已设置：{p}", "  Already set: {p}"));
        return Ok(false);
    }
    println!(
        "{}",
        ictl_l10n::tr!(
            "下载开发者镜像和 WebDriverAgent、登录 Apple ID、发通知都要访问外网；直连访问 GitHub 慢的话可以设代理。",
            "Downloading the developer disk image and WebDriverAgent, signing in to the Apple ID and sending notifications go out to the internet; set a proxy if GitHub is slow to reach directly."
        )
    );
    if !ask_yes(ictl_l10n::tr!("要用代理吗？", "Use a proxy?"), current.is_some())? {
        let a = env.set("HTTP_PROXY", None)?;
        return Ok(env.set("HTTPS_PROXY", None)? | a);
    }
    let prompt = match &current {
        Some(p) => ictl_l10n::trf!("代理地址（回车保留 {p}）: ", "Proxy URL (Enter keeps {p}): "),
        None => {
            ictl_l10n::tr!("代理地址，例如 http://127.0.0.1:7890 : ", "Proxy URL, e.g. http://127.0.0.1:7890 : ").into()
        }
    };
    let mut url = crate::read_line(&prompt)?;
    if url.is_empty() {
        url = current.unwrap_or_default();
    }
    if !url.is_empty() && !url.contains("://") {
        url = format!("http://{url}");
    }
    let a = env.set("HTTP_PROXY", Some(&url))?;
    Ok(env.set("HTTPS_PROXY", Some(&url))? | a)
}

async fn wait_daemon(socket: &Path, timeout: Duration) -> bool {
    crate::wait_until(timeout, Duration::from_secs(1), || async { crate::snapshot(socket).await.is_ok() }).await
}

/// Makes sure the daemon runs (as a service if the user agrees); `restart` reloads changed settings
async fn daemon(cli: &Cli, restart: bool) -> anyhow::Result<()> {
    let running = crate::snapshot(&cli.socket).await.is_ok();
    match (running, service::find()) {
        (true, Some(s)) if restart => {
            println!(
                "{}",
                ictl_l10n::tr!("  重启守护进程，让新设置生效…", "  Restarting the daemon so the new settings apply…")
            );
            service::restart(&s)?;
            // stopping takes up to half a minute (it ends the phone's sessions first)
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !wait_daemon(&cli.socket, Duration::from_secs(60)).await {
                bail!(
                    "{}",
                    ictl_l10n::tr!(
                        "守护进程 1 分钟内没起来，运行 iphone-control service status 看看",
                        "The daemon didn't come up within a minute; see iphone-control service status"
                    )
                );
            }
        }
        (true, Some(s)) => println!("{}", ictl_l10n::trf!("  在运行：{}", "  Running: {}", s.describe())),
        (true, None) => {
            println!(
                "{}",
                ictl_l10n::tr!(
                    "  守护进程是手动启动的（不是后台服务）。要改成登录后自动启动、崩溃后自动重启的后台服务，先把它停掉（在它的终端里按 Ctrl-C），再运行 iphone-control service install。",
                    "  The daemon was started by hand (not as a service). To run it as a service that starts at login and restarts after a crash, stop it (Ctrl-C in its terminal), then run iphone-control service install."
                )
            );
            if restart {
                println!(
                    "{}",
                    ictl_l10n::tr!("  改过的设置要重启它才生效。", "  Restart it for the changed settings to apply.")
                );
            }
        }
        (false, Some(s)) => {
            println!("{}", ictl_l10n::trf!("  启动 {}…", "  Starting {}…", s.describe()));
            service::start(&s)?;
            if !wait_daemon(&cli.socket, Duration::from_secs(60)).await {
                bail!(
                    "{}",
                    ictl_l10n::tr!(
                        "守护进程 1 分钟内没起来，运行 iphone-control service status 看看",
                        "The daemon didn't come up within a minute; see iphone-control service status"
                    )
                );
            }
            println!("{}", ictl_l10n::tr!("  已启动", "  Started"));
        }
        (false, None) => {
            println!(
                "{}",
                ictl_l10n::tr!(
                    "  守护进程负责和手机保持连接，并在签名过期前自动重签。建议装成后台服务：登录后自动启动，崩溃后自动重启。",
                    "  The daemon keeps the connection to the phone and re-signs before the signature expires. A background service is recommended: it starts at login and restarts after a crash."
                )
            );
            if ask_yes(ictl_l10n::tr!("  装成后台服务？", "  Set it up as a service?"), true)? {
                let s = service::install()?;
                println!("{}", ictl_l10n::trf!("  已安装并启动：{}", "  Installed and started: {}", s.describe()));
                if !wait_daemon(&cli.socket, Duration::from_secs(60)).await {
                    bail!(
                        "{}",
                        ictl_l10n::tr!(
                            "守护进程 1 分钟内没起来，运行 iphone-control service status 看看",
                            "The daemon didn't come up within a minute; see iphone-control service status"
                        )
                    );
                }
            } else {
                println!(
                    "{}",
                    ictl_l10n::tr!(
                        "  在另一个终端运行 iphone-control daemon，这里会等它起来…",
                        "  Run iphone-control daemon in another terminal; waiting for it…"
                    )
                );
                if !wait_daemon(&cli.socket, Duration::from_secs(600)).await {
                    bail!("{}", ictl_l10n::tr!("10 分钟内没等到守护进程", "The daemon didn't start within 10 minutes"));
                }
            }
        }
    }
    Ok(())
}

async fn reinstall_service(cli: &Cli) -> anyhow::Result<()> {
    match service::find() {
        Some(s) => {
            println!("{}", ictl_l10n::trf!("后台服务：{}", "Service: {}", s.describe()));
            service::restart(&s)?;
            wait_daemon(&cli.socket, Duration::from_secs(60)).await;
            println!("{}", ictl_l10n::tr!("已重启", "Restarted"));
            Ok(())
        }
        None => daemon(cli, false).await,
    }
}

async fn passcode(cli: &Cli, force: bool) -> anyhow::Result<()> {
    let cfg = ictl_daemon::Config::from_env(&cli.data_dir);
    let device = crate::target_device(&cli.socket, None).await?;
    let set = cfg.passcode_file_for(&device.udid).is_file();
    println!(
        "{}",
        ictl_l10n::tr!(
            "  手机锁屏时，操作前用锁屏密码自动解锁。不设的话，锁屏后要有人手动解锁。",
            "  When the phone is locked, actions unlock it with the passcode first. Without it, someone has to unlock it by hand."
        )
    );
    if set && !force {
        println!("{}", ictl_l10n::tr!("  这台设备已设置", "  Already set for this device"));
        return Ok(());
    }
    let what = if set {
        choose(
            ictl_l10n::tr!("  这台设备已设置自动解锁密码：", "  This device has an auto-unlock passcode:"),
            &[
                ictl_l10n::tr!("保留", "Keep it"),
                ictl_l10n::tr!("重新输入", "Enter it again"),
                ictl_l10n::tr!("删除（关闭自动解锁）", "Delete it (auto-unlock off)"),
            ],
            0,
        )?
    } else if ask_yes(ictl_l10n::tr!("  设置自动解锁？", "  Set up auto-unlock?"), false)? {
        1
    } else {
        0
    };
    match what {
        1 => crate::passcode(&cli.data_dir, &cli.socket, crate::PasscodeAction::Set { device: None }).await,
        2 => {
            crate::passcode(&cli.data_dir, &cli.socket, crate::PasscodeAction::Clear { device: None, all: false }).await
        }
        _ => Ok(()),
    }
}

/// Asks for a notification channel. Returns whether the file changed, and the settings to test if there are any
fn notifications(env: &mut EnvFile, force: bool) -> anyhow::Result<(bool, Option<ictl_daemon::config::NotifyConfig>)> {
    let bark = env.get("BARK_KEY");
    let tg = env.get("TG_BOT_TOKEN").zip(env.get("TG_CHAT_ID"));
    println!(
        "{}",
        ictl_l10n::tr!(
            "  重签失败、Apple ID 会话过期、手机离线、重启后要输密码时推送提醒。",
            "  Alerts when re-signing fails, the Apple ID session expires, the phone goes offline or needs its passcode after a restart."
        )
    );
    if !force && (bark.is_some() || tg.is_some()) {
        println!("{}", ictl_l10n::tr!("  已配置", "  Already configured"));
        return Ok((false, None));
    }
    let pick = choose(
        ictl_l10n::tr!("  用哪种方式推送？", "  How should alerts reach you?"),
        &[
            ictl_l10n::tr!("不需要", "None"),
            ictl_l10n::tr!("Bark（iPhone 上的 App）", "Bark (an iPhone app)"),
            "Telegram",
        ],
        if bark.is_some() {
            1
        } else if tg.is_some() {
            2
        } else {
            0
        },
    )?;
    let mut changed = false;
    match pick {
        1 => {
            let key = crate::read_secret(ictl_l10n::tr!(
                "  Bark 的 key（App 里复制的地址里那一段，输入不显示）: ",
                "  Bark key (the part in the URL the app shows, hidden): "
            ))?;
            if key.is_empty() {
                bail!("{}", ictl_l10n::tr!("没有输入 key", "No key entered"));
            }
            let server = crate::read_line(ictl_l10n::tr!(
                "  Bark 服务器（回车用 https://api.day.app）: ",
                "  Bark server (Enter for https://api.day.app): "
            ))?;
            changed |= env.set("BARK_KEY", Some(&key))?;
            changed |= env.set("BARK_SERVER", Some(if server.is_empty() { "https://api.day.app" } else { &server }))?;
            changed |= env.set("TG_BOT_TOKEN", None)? | env.set("TG_CHAT_ID", None)?;
        }
        2 => {
            let token = crate::read_secret(ictl_l10n::tr!(
                "  机器人 token（@BotFather 给的，输入不显示）: ",
                "  Bot token (from @BotFather, hidden): "
            ))?;
            let chat = crate::read_line(ictl_l10n::tr!(
                "  chat id（给机器人发条消息后，用 @userinfobot 查）: ",
                "  Chat id (message the bot first, then ask @userinfobot): "
            ))?;
            if token.is_empty() || chat.is_empty() {
                bail!("{}", ictl_l10n::tr!("token 和 chat id 都要填", "Both the token and the chat id are needed"));
            }
            changed |= env.set("TG_BOT_TOKEN", Some(&token))? | env.set("TG_CHAT_ID", Some(&chat))?;
            changed |= env.set("BARK_KEY", None)?;
        }
        _ => {
            changed |= env.set("BARK_KEY", None)? | env.set("TG_BOT_TOKEN", None)? | env.set("TG_CHAT_ID", None)?;
            return Ok((changed, None));
        }
    }
    if !changed {
        return Ok((false, None));
    }
    Ok((
        true,
        Some(ictl_daemon::config::NotifyConfig {
            tg_bot_token: env.get("TG_BOT_TOKEN"),
            tg_chat_id: env.get("TG_CHAT_ID"),
            bark_server: env.get("BARK_SERVER").unwrap_or_else(|| "https://api.day.app".into()),
            bark_key: env.get("BARK_KEY"),
        }),
    ))
}

async fn notify_step(cli: &Cli, env: &mut EnvFile, force: bool) -> anyhow::Result<()> {
    let (changed, test) = notifications(env, force)?;
    if changed {
        env.save()?;
        daemon(cli, true).await?;
    }
    if let Some(t) = test {
        send_test(&t).await;
    }
    Ok(())
}

async fn send_test(cfg: &ictl_daemon::config::NotifyConfig) {
    let ok = ictl_daemon::notify::send(
        cfg,
        ictl_l10n::tr!("[测试] iphone-control 通知已配置", "[test] iphone-control notifications are set up"),
        ictl_l10n::tr!("收到这条说明通知能用。", "If this arrived, notifications work."),
    )
    .await;
    println!(
        "{}",
        if ok > 0 {
            ictl_l10n::tr!("  已发一条测试通知，看看手机收到没有", "  Sent a test notification; check that it arrived")
        } else {
            ictl_l10n::tr!(
                "  测试通知没发出去：检查 key 或 token、代理，之后可以用 iphone-control notify-test 再试",
                "  The test notification wasn't sent: check the key or token and the proxy, then try iphone-control notify-test"
            )
        }
    );
}

/// Registers the MCP server with the AI clients found on PATH
fn register_mcp(cli: &Cli, force: bool) -> anyhow::Result<()> {
    let exe = service::stable_exe()?;
    let socket = cli.socket.to_string_lossy().into_owned();
    let exe = exe.to_string_lossy().into_owned();
    let mut env_args: Vec<String> = Vec::new();
    if ictl_l10n::is_en() {
        env_args.push(format!("{}=en", ictl_l10n::ENV));
    }
    if ictl_mcp::dev_tools_enabled() {
        env_args.push("IPHONE_CONTROL_DEV_TOOLS=1".into());
    }
    let server = [exe.as_str(), "--socket", socket.as_str(), "mcp"];
    let mut any = false;
    for backend in crate::agent::Backend::ALL {
        let Some(program) = backend.find() else { continue };
        any = true;
        let name = backend.name();
        let registered = registered(backend);
        if registered && !force {
            println!("{}", ictl_l10n::trf!("  {name}：已注册", "  {name}: already registered"));
            continue;
        }
        if !ask_yes(&ictl_l10n::trf!("  注册到 {name}？", "  Register with {name}?"), true)? {
            continue;
        }
        let mut cmd = std::process::Command::new(&program);
        match backend {
            crate::agent::Backend::Claude => {
                let _ = std::process::Command::new(&program)
                    .args(["mcp", "remove", "iphone", "-s", "user"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                cmd.args(["mcp", "add", "iphone", "-s", "user"]);
                for e in &env_args {
                    cmd.args(["-e", e]);
                }
            }
            crate::agent::Backend::Codex => {
                let _ = std::process::Command::new(&program)
                    .args(["mcp", "remove", "iphone"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                cmd.args(["mcp", "add", "iphone"]);
                for e in &env_args {
                    cmd.args(["--env", e]);
                }
            }
        }
        cmd.arg("--").args(server).stdout(std::process::Stdio::null());
        match cmd.status() {
            Ok(s) if s.success() => println!(
                "{}",
                ictl_l10n::trf!(
                    "  已注册到 {name}，新开的会话里可以用",
                    "  Registered with {name}; available in new sessions"
                )
            ),
            _ => println!(
                "{}",
                ictl_l10n::trf!(
                    "  注册到 {name} 失败，可以手动运行：iphone-control mcp-config",
                    "  Registering with {name} failed; do it by hand: iphone-control mcp-config"
                )
            ),
        }
    }
    if !any {
        println!(
            "{}",
            ictl_l10n::trf!(
                "  这台机器上没找到 Claude Code 或 Codex。装好后运行 iphone-control setup --reconfigure 选「注册 MCP」；其他 MCP 客户端用这条命令注册：\n  {}",
                "  Neither Claude Code nor Codex was found. Once one is installed, run iphone-control setup --reconfigure and pick MCP registration; for other MCP clients the server command is:\n  {}",
                server.join(" ")
            )
        );
    }
    Ok(())
}

/// Whether the client already has an `iphone` server: read from its config file, since asking the CLI also
/// connects to every server and takes seconds
fn registered(backend: crate::agent::Backend) -> bool {
    let home = envfile::home();
    match backend {
        crate::agent::Backend::Claude => std::fs::read_to_string(home.join(".claude.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .is_some_and(|v| v["mcpServers"]["iphone"].is_object()),
        crate::agent::Backend::Codex => {
            let dir = std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".codex"));
            std::fs::read_to_string(dir.join("config.toml"))
                .is_ok_and(|t| t.lines().any(|l| l.trim() == "[mcp_servers.iphone]"))
        }
    }
}
