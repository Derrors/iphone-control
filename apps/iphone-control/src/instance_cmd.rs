//! `instance add / list / remove / check`: named instances, one daemon per device (docs/dev/multi-instance-plan.md
//! 4.4, 5.1, 5.2).
//!
//! Everything that changes the registry or the services takes `registry.lock` (in the registry directory) first, then
//! the maintenance lock's shared lease: never during an update. The daemons never take `registry.lock`; while it is
//! held only `Request::Daemon` and `PinCurrent` are sent to them.
//!
//! `add` goes: confirm what the default instance controls (outside the locks) → the locks, and the same read again →
//! the checks (name, device, port, the data root, the anisette state's origin) → a reservation (`<n>.pending`, which
//! the default instance already keeps clear of) → the default instance pinned to the device it has, by compare-and-
//! set against what was confirmed → `<n>.env` written, the reservation gone → the services started and checked, or
//! stopped and the registration removed again.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use ictl_daemon::instance::{self, Managed, Pending, Registered};
use ictl_daemon::protocol::{Request, Response};

use crate::client::{ask, snapshot};

/// The first port tried for a new instance's control page (the default instance's is 8200)
const FIRST_PORT: u16 = 8201;
/// How long a new instance's daemon and viewer get to come up
const START_WAIT: Duration = Duration::from_secs(60);
/// How long `registry.lock` is waited for
const LOCK_WAIT: Duration = Duration::from_secs(10);

#[derive(clap::Subcommand)]
pub enum InstanceAction {
    #[command(about = ictl_l10n::tr!("添加一个命名实例：给它一台设备、一个控制台端口，装好它的服务并启动。默认实例会固定在它正在控制的那台", "Add a named instance: give it a device and a control page port, set up its services and start them. The default instance is pinned to the device it controls now"))]
    Add {
        name: String,
        #[arg(long, value_name = "UDID", required_unless_present = "resume")]
        device: Option<String>,
        #[arg(long, help = ictl_l10n::tr!("控制台端口；不给就从 8201 起找一个空的", "The control page's port; the first free one from 8201 if not given"))]
        port: Option<u16>,
        #[arg(long, help = ictl_l10n::tr!("设备现在没接在 USB 上也登记（服务起来后等它接上）", "Register even if the device isn't on USB now (the service waits for it)"))]
        force: bool,
        #[arg(short, long, help = ictl_l10n::tr!("不询问", "Don't ask"))]
        yes: bool,
        #[arg(long, help = ictl_l10n::tr!("继续一次中断的 instance add（只剩 <名字>.pending 时）", "Go on with an instance add that was interrupted (only <name>.pending is left)"))]
        resume: bool,
    },
    #[command(about = ictl_l10n::tr!("列出实例：进程能不能连上、有没有拿到设备、设备是否就绪", "List the instances: whether the process answers, holds its device, and the device is ready"))]
    List,
    #[command(about = ictl_l10n::tr!("停掉并移除一个命名实例的服务和登记（确认停下才删）；--purge 连它的数据目录一起删", "Stop and remove a named instance's services and registration (removed once confirmed stopped); --purge also deletes its data directory"))]
    Remove {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// For the services: whether the registration can be started (exit 0), without changing anything
    #[command(hide = true)]
    Check { name: String },
}

pub async fn run(default_socket: &Path, named: Option<&str>, action: InstanceAction) -> anyhow::Result<()> {
    if let (Some(n), false) = (named, matches!(action, InstanceAction::Check { .. } | InstanceAction::List)) {
        bail!(
            "{}",
            ictl_l10n::trf!(
                "instance 命令在默认实例上运行，不带 --instance（现在是 {n}）",
                "The instance commands run on the default instance, without --instance (it is {n} now)"
            )
        );
    }
    // run as a named instance (`--instance n status --all`), the socket given is that one's
    let default_socket = match named {
        Some(_) => ictl_daemon::protocol::default_socket(),
        None => default_socket.to_owned(),
    };
    let default_socket = default_socket.as_path();
    match action {
        InstanceAction::Add { name, device, port, force, yes, resume } => {
            add(default_socket, &name, device.as_deref(), port, force, yes, resume).await
        }
        InstanceAction::List => list(default_socket).await,
        InstanceAction::Remove { name, purge } => remove(&name, purge).await,
        InstanceAction::Check { name } => check(&name),
    }
}

/// `instance check <n>`: the registration is there, whole, in a format this program knows. Read only, also during
/// an update. Exit code 1 otherwise (systemd's `ExecCondition` then skips the unit)
fn check(name: &str) -> anyhow::Result<()> {
    let registry = instance::registry_dir()?;
    match instance::read(&registry, name) {
        Ok(Some(_)) if instance::valid_name(name) => Ok(()),
        Ok(_) => {
            eprintln!(
                "{}",
                ictl_l10n::trf!(
                    "实例 {name} 没有登记（或还在添加中）",
                    "The instance {name} isn't registered (or is still being added)"
                )
            );
            std::process::exit(1)
        }
        Err(e) => {
            eprintln!("{}: {e}", registry.join(format!("{name}.env")).display());
            std::process::exit(1)
        }
    }
}

/// `registry.lock`, waited for a while
fn lock_registry(dir: &Path) -> anyhow::Result<File> {
    instance::lock_registry(dir, LOCK_WAIT).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::anyhow!(
                "{}",
                ictl_l10n::tr!(
                    "另一个 instance 命令或升级正在改登记，稍后再试",
                    "Another instance command or an update is changing the registry; try again shortly"
                )
            )
        } else {
            e.into()
        }
    })
}

/// What the default instance reports, for `add` to confirm and compare-and-set against
#[derive(Debug, Clone, PartialEq, Eq)]
struct DefaultState {
    /// The device it controls, or else has selected
    current: Option<String>,
    token: String,
    data_dir: PathBuf,
    /// The registry it reads
    registry: PathBuf,
}

async fn default_state(socket: &Path) -> anyhow::Result<DefaultState> {
    let s = snapshot(socket).await?;
    let id = s.instance.context(ictl_l10n::tr!(
        "默认实例的守护进程是旧版本，不支持命名实例：先升级并重启它",
        "The default instance's daemon is an older version without named instances: update and restart it first"
    ))?;
    ensure!(
        id.name == instance::DEFAULT,
        "{}",
        ictl_l10n::trf!(
            "{} 上的是实例 {}，不是默认实例",
            "{} is the instance {}, not the default one",
            socket.display(),
            id.name
        )
    );
    Ok(DefaultState {
        current: s.device.map(|d| d.udid).or(s.pinned),
        token: format!("{}:{}", id.boot_id, id.selection_generation),
        data_dir: id.data_dir,
        registry: id.registry,
    })
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// With named instances every process must find the same registry and data root (docs/dev/multi-instance-plan.md
/// 3.2, 10.5): the settings directory the services see (`~/.config`, no `XDG_CONFIG_HOME` there), the data directory
/// as worked out here, the host directory of the maintenance lock, and the one the default daemon really uses
fn check_root(reported: &Path) -> anyhow::Result<PathBuf> {
    let home = crate::envfile::home();
    let config = instance::config_dir();
    ensure!(
        same_dir(&config, &home.join(".config/iphone-control")),
        "{}",
        ictl_l10n::trf!(
            "设置目录是 {}（XDG_CONFIG_HOME）：有命名实例时要用 ~/.config/iphone-control，后台服务只认它",
            "The settings directory is {} (XDG_CONFIG_HOME): with named instances it must be ~/.config/iphone-control, the only one the services see",
            config.display()
        )
    );
    let mine = ictl_daemon::protocol::default_data_dir();
    let host = ictl_daemon::maintenance::host_dir()?;
    for (what, dir) in [
        (ictl_l10n::tr!("维护锁的目录", "the maintenance lock's directory"), &host),
        (
            ictl_l10n::tr!("默认守护进程在用的数据目录", "the data directory the default daemon uses"),
            &reported.to_owned(),
        ),
    ] {
        ensure!(
            same_dir(&mine, dir),
            "{}",
            ictl_l10n::trf!(
                "数据目录 {} 和{what} {} 不是同一个（XDG_DATA_HOME 或 ICTL_DATA_DIR 改过）：有命名实例时不支持别的数据目录",
                "The data directory {} isn't {what} {} (XDG_DATA_HOME or ICTL_DATA_DIR was changed): with named instances no other data directory is supported",
                mine.display(),
                dir.display()
            )
        );
    }
    Ok(mine)
}

/// The anisette state is used by every instance once there are several: one of unknown origin first confirmed or
/// provisioned again (docs/dev/multi-instance-plan.md 4.2, 10.4)
fn check_anisette(root: &Path) -> anyhow::Result<()> {
    use ictl_sign::account::OriginCheck;
    match ictl_daemon::Config::from_env(root).account().anisette_origin() {
        Ok(OriginCheck::Fresh | OriginCheck::Known) => Ok(()),
        Ok(OriginCheck::Unknown) => bail!(
            "{}",
            ictl_l10n::tr!(
                "账号的 anisette 状态是旧版本生成的，不知道来自哪个地址：先运行 iphone-control login --reprovision-anisette（确知就是现在配置的地址生成的，也可以 --confirm-anisette-origin）",
                "The account's anisette state was made by an older version, from an address not recorded: run iphone-control login --reprovision-anisette first (or --confirm-anisette-origin if you know it came from the address configured now)"
            )
        ),
        Ok(OriginCheck::Mismatch { recorded, configured }) => bail!(
            "{}",
            ictl_l10n::trf!(
                "账号的 anisette 状态来自 {recorded}，现在配置的是 {configured}：先改回去，或者 login --reprovision-anisette",
                "The account's anisette state came from {recorded}, the address configured now is {configured}: change it back, or run login --reprovision-anisette"
            )
        ),
        Err(e) => Err(e.into()),
    }
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The default instance's control page port, as its settings say
fn default_port() -> u16 {
    std::env::var(ictl_daemon::protocol::env::VIEWER_PORT).ok().and_then(|p| p.parse().ok()).unwrap_or(8200)
}

#[allow(clippy::too_many_arguments)]
async fn add(
    socket: &Path,
    name: &str,
    device: Option<&str>,
    port: Option<u16>,
    force: bool,
    yes: bool,
    resume: bool,
) -> anyhow::Result<()> {
    ensure!(
        instance::valid_name(name),
        "{}",
        ictl_l10n::trf!(
            "实例名 {name:?} 不对：小写字母、数字和 -，以字母或数字开头，最长 32 个字符，不能是 default",
            "The instance name {name:?} isn't valid: lower-case letters, digits and -, starting with a letter or digit, 32 at most, not default"
        )
    );
    let registry = instance::registry_dir()?;
    let pending_before = if resume { Some(read_pending(&registry, name)?) } else { None };
    let udid = match (device, &pending_before) {
        (Some(d), Some(p)) if !d.eq_ignore_ascii_case(&p.udid) => {
            bail!(
                "{}",
                ictl_l10n::trf!("{name}.pending 预约的是 {}，不是 {d}", "{name}.pending reserved {}, not {d}", p.udid)
            )
        }
        (Some(d), _) => d.trim().to_owned(),
        (None, Some(p)) => p.udid.clone(),
        (None, None) => bail!("--device"),
    };
    // 1. outside the locks: what the default instance does now, confirmed
    let seen = default_state(socket).await?;
    let current = match &seen.current {
        Some(c) if c.eq_ignore_ascii_case(&udid) => bail!(
            "{}",
            ictl_l10n::trf!(
                "默认实例正在控制 {udid}：先用 iphone-control select 让它控制另一台，再把这台给新实例",
                "The default instance controls {udid}: have it control another device first (iphone-control select), then give this one to the new instance"
            )
        ),
        Some(c) => c.clone(),
        None => bail!(
            "{}",
            ictl_l10n::tr!(
                "默认实例现在没在控制设备，也没选定：先用 iphone-control select 给它选一台（有命名实例时它只控制选定的）",
                "The default instance controls no device and has none selected: select one for it first (iphone-control select; beside named instances it controls only the one selected)"
            )
        ),
    };
    println!(
        "{}",
        ictl_l10n::trf!(
            "默认实例会固定控制 {current}；新实例 {name} 控制 {udid}。",
            "The default instance stays on {current} for good; the new instance {name} controls {udid}."
        )
    );
    if !yes {
        ensure!(
            std::io::IsTerminal::is_terminal(&std::io::stdin()),
            "{}",
            ictl_l10n::tr!(
                "没有终端可以确认：确认无误就加 --yes",
                "No terminal to confirm on: add --yes if this is right"
            )
        );
        print!("{}", ictl_l10n::tr!("继续？[y/N] ", "Go on? [y/N] "));
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        ensure!(matches!(line.trim(), "y" | "Y" | "yes"), "{}", ictl_l10n::tr!("没有添加", "Not added"));
    }
    // 2. the locks, and the same again
    let _registry_lock = lock_registry(&registry)?;
    let _maintenance = ictl_daemon::maintenance::shared()
        .context(ictl_l10n::tr!("正在升级，稍后再添加", "An update is under way; add it later"))?;
    let now = default_state(socket).await?;
    ensure!(
        now == seen,
        "{}",
        ictl_l10n::tr!(
            "确认之后默认实例的设备变了，没有添加；重新运行",
            "The default instance's device changed after it was confirmed; not added. Run it again"
        )
    );
    // 3. the checks
    let root = check_root(&now.data_dir)?;
    // the default daemon must read the registry written here: one started with another XDG_CONFIG_HOME would miss
    // the instance, and choose its device as before
    ensure!(
        same_dir(&now.registry, &registry),
        "{}",
        ictl_l10n::trf!(
            "默认守护进程读的实例登记是 {}，这里写的是 {}（设置目录不同）：用同样的设置重启它再添加",
            "The default daemon reads the instance registry {}, this writes {} (another settings directory): restart it with the same settings, then add",
            now.registry.display(),
            registry.display()
        )
    );
    check_anisette(&root)?;
    let scan = instance::scan(&registry)?;
    if scan.registered.iter().any(|r| r.name == name) {
        bail!("{}", ictl_l10n::trf!("已经有实例 {name}", "There is an instance {name} already"));
    }
    let others_pending: Vec<&Pending> = scan.pending.iter().filter(|p| p.name != name).collect();
    if !resume && scan.pending.iter().any(|p| p.name == name) {
        bail!(
            "{}",
            ictl_l10n::trf!(
                "{name} 有一次没做完的添加：instance add --resume {name} 继续，或 instance remove {name} 清掉",
                "{name} has an add that didn't finish: continue it with instance add --resume {name}, or clear it with instance remove {name}"
            )
        );
    }
    if let Some(by) = scan.reserved_by(&udid).filter(|n| *n != name) {
        bail!("{}", ictl_l10n::trf!("{udid} 已经给了实例 {by}", "{udid} belongs to the instance {by} already"));
    }
    let taken: Vec<u16> = scan
        .registered
        .iter()
        .map(|r| r.managed.port)
        .chain(others_pending.iter().map(|p| p.port))
        .chain([default_port()])
        .collect();
    let port = match port.or(pending_before.as_ref().map(|p| p.port)) {
        Some(p) => {
            ensure!(
                !taken.contains(&p),
                "{}",
                ictl_l10n::trf!("端口 {p} 已经有实例在用", "The port {p} is another instance's")
            );
            ensure!(port_free(p), "{}", ictl_l10n::trf!("端口 {p} 被占着", "The port {p} is in use"));
            p
        }
        None => (FIRST_PORT..FIRST_PORT + 100)
            .find(|p| !taken.contains(p) && port_free(*p))
            .context(ictl_l10n::tr!("找不到空的端口：用 --port 指定", "No free port found: give one with --port"))?,
    };
    if !force {
        let connected = ictl_device::list_devices().await.unwrap_or_default();
        ensure!(
            connected.iter().any(|d| d.udid.eq_ignore_ascii_case(&udid)),
            "{}",
            ictl_l10n::trf!(
                "{udid} 没接在 USB 上（iphone-control devices 列出接着的）；设备现在不在也要登记就加 --force",
                "{udid} isn't on USB (iphone-control devices lists what is); to register it while it isn't, add --force"
            )
        );
    }
    // 4. the reservation (from here on the default instance keeps clear of the device), and the anchor the first time
    let pending = Pending { name: name.into(), udid: udid.clone(), port, started: unix_now() };
    write_file(&registry, &format!("{name}.pending"), &serde_json::to_vec_pretty(&pending)?)?;
    if instance::anchor()?.is_none() {
        instance::write_anchor(&instance::Anchor { registry: registry.clone(), root: root.clone() })?;
    }
    // 5. the default instance pinned to what it has, if nothing changed since it was confirmed
    let token =
        match ask(socket, Request::PinCurrent { udid: current.clone(), expect_token: now.token.clone() }).await? {
            Response::Text { text, .. } => text,
            Response::Error { message, .. } => {
                let _ = std::fs::remove_file(registry.join(format!("{name}.pending")));
                bail!("{message}");
            }
            Response::Image { .. } => bail!("unexpected"),
        };
    // 6. the registration
    let after = default_state(socket).await?;
    if after.token != token {
        let _ = std::fs::remove_file(registry.join(format!("{name}.pending")));
        bail!(
            "{}",
            ictl_l10n::tr!(
                "默认实例的选择又变了，没有添加",
                "The default instance's selection changed again; not added"
            )
        );
    }
    let (data_dir, inst_socket) = instance::paths_for(&root, name);
    let reg = Registered {
        name: name.into(),
        managed: Managed {
            socket: inst_socket.clone(),
            data_dir,
            udid: udid.clone(),
            port,
            id: uuid::Uuid::new_v4().simple().to_string(),
        },
        settings: Vec::new(),
    };
    let text = instance::render(&reg).context("a path this format can't hold")?;
    write_file(&registry, &format!("{name}.env"), text.as_bytes())?;
    std::fs::remove_file(registry.join(format!("{name}.pending")))?;
    println!(
        "{}",
        ictl_l10n::trf!(
            "已登记 {name}，默认实例已固定在 {current}",
            "Registered {name}; the default instance is pinned to {current}"
        )
    );
    // 7. the services, started and checked; 8. stopped and the registration removed again when they don't come up
    let started = match crate::service::instance_install(name) {
        Ok(_) => wait_started(&reg, &root, force).await,
        Err(e) => Err(e),
    };
    if let Err(e) = started {
        eprintln!("{e:#}");
        if crate::service::instance_stop(name) {
            let left = instance::scan(&registry).map(|s| s.registered.len() > 1).unwrap_or(true);
            let _ = crate::service::instance_remove_files(name, left);
            let _ = std::fs::remove_file(registry.join(format!("{name}.env")));
            bail!(
                "{}",
                ictl_l10n::trf!(
                    "{name} 没起来，已撤回登记；默认实例仍固定在 {current}",
                    "{name} didn't come up and its registration was taken back; the default instance stays pinned to {current}"
                )
            );
        }
        bail!(
            "{}",
            ictl_l10n::trf!(
                "{name} 没起来，服务也没能停下，登记留着：查看后用 instance remove {name}",
                "{name} didn't come up and its services couldn't be stopped, so the registration stays: look into it, then instance remove {name}"
            )
        );
    }
    println!(
        "{}",
        ictl_l10n::trf!(
            "实例 {name} 已启动：控制台 http://127.0.0.1:{port}/（令牌和默认实例一样），MCP：iphone-control --instance {name} mcp-config",
            "The instance {name} is running: control page http://127.0.0.1:{port}/ (the same tokens as the default one), MCP: iphone-control --instance {name} mcp-config"
        )
    );
    Ok(())
}

fn read_pending(registry: &Path, name: &str) -> anyhow::Result<Pending> {
    let p = registry.join(format!("{name}.pending"));
    let b = std::fs::read(&p).with_context(|| {
        ictl_l10n::trf!("{} 不在：没有要继续的添加", "{} isn't there: nothing to continue", p.display())
    })?;
    Ok(serde_json::from_slice(&b)?)
}

fn write_file(dir: &Path, name: &str, data: &[u8]) -> anyhow::Result<()> {
    instance::write_file(dir, name, data).with_context(|| dir.join(name).display().to_string())
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The new instance is up: its daemon answers as that instance, of this version, with its device (held, or with
/// `force` maybe not connected yet), and its control page answers on its port, reaching that daemon
async fn wait_started(reg: &Registered, root: &Path, force: bool) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + START_WAIT;
    let m = &reg.managed;
    let mut why = String::new();
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let s = match tokio::time::timeout(Duration::from_secs(2), snapshot(&m.socket)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                why = e.to_string();
                continue;
            }
            Err(_) => continue,
        };
        let ok_id = s.instance.as_ref().is_some_and(|i| i.name == reg.name && i.id == m.id);
        let ok_version = s.version.as_deref() == Some(env!("CARGO_PKG_VERSION"));
        let held = s.device.as_ref().is_some_and(|d| d.udid.eq_ignore_ascii_case(&m.udid));
        let waiting = force && s.state == ictl_daemon::machine::DeviceState::Absent;
        if !(ok_id && ok_version && (held || waiting)) {
            why = ictl_l10n::trf!("守护进程状态：{}", "the daemon's state: {}", s.state.describe());
            continue;
        }
        match viewer_reaches(m.port, root).await {
            Ok(name) if name == reg.name => return Ok(()),
            Ok(other) => {
                why = ictl_l10n::trf!("端口 {} 上的控制台是 {other} 的", "the control page on {} is {other}'s", m.port)
            }
            Err(e) => why = e.to_string(),
        }
    }
    bail!("{}", ictl_l10n::trf!("{} 秒内没起来：{why}", "not up within {} s: {why}", START_WAIT.as_secs()))
}

/// The instance whose daemon the control page on `port` reaches (logged in with the owner token every viewer shares)
async fn viewer_reaches(port: u16, root: &Path) -> anyhow::Result<String> {
    let token = crate::viewer::auth::get(root, crate::viewer::auth::OWNER)?.token;
    let client = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(3)).build()?;
    let v: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/api/daemon"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    v["instance"]["name"].as_str().map(str::to_owned).context("no instance in /api/daemon")
}

/// `instance list`
async fn list(default_socket: &Path) -> anyhow::Result<()> {
    let registry = instance::registry_dir()?;
    let scan = instance::scan(&registry)?;
    let mut rows = vec![(instance::DEFAULT.to_owned(), default_socket.to_owned(), None, default_port())];
    rows.extend(
        scan.registered
            .iter()
            .map(|r| (r.name.clone(), r.managed.socket.clone(), Some(r.managed.udid.clone()), r.managed.port)),
    );
    for (name, socket, udid, port) in rows {
        let line = match tokio::time::timeout(Duration::from_secs(2), snapshot(&socket)).await {
            Ok(Ok(s)) => {
                let device =
                    s.device.as_ref().map(ictl_daemon::core::device_label).or(udid).unwrap_or_else(|| "-".into());
                format!("{name:<10} {device:<40} {:<24} :{port}", s.state.describe())
            }
            _ => format!(
                "{name:<10} {:<40} {:<24} :{port}",
                udid.unwrap_or_else(|| "-".into()),
                ictl_l10n::tr!("连不上（没在运行）", "not answering (not running)")
            ),
        };
        println!("{line}");
    }
    for p in &scan.pending {
        println!(
            "{}",
            ictl_l10n::trf!(
                "{:<10} {:<40} 添加没做完：instance add --resume {} 继续，或 instance remove {} 清掉",
                "{:<10} {:<40} add not finished: instance add --resume {} to continue, or instance remove {} to clear it",
                p.name,
                p.udid,
                p.name,
                p.name
            )
        );
    }
    Ok(())
}

/// `instance remove <n>`: stopped first, and only once confirmed stopped are its services and registration removed
/// (with `purge`, its data directory too). What it left on its devices goes as it does at `service stop`
async fn remove(name: &str, purge: bool) -> anyhow::Result<()> {
    ensure!(
        instance::valid_name(name),
        "{}",
        ictl_l10n::trf!("实例名 {name:?} 不对", "The instance name {name:?} isn't valid")
    );
    let registry = instance::registry_dir()?;
    let _registry_lock = lock_registry(&registry)?;
    let _maintenance = ictl_daemon::maintenance::shared()
        .context(ictl_l10n::tr!("正在升级，稍后再移除", "An update is under way; remove it later"))?;
    let reg = instance::read(&registry, name)?;
    let pending = registry.join(format!("{name}.pending"));
    let Some(reg) = reg else {
        ensure!(pending.exists(), "{}", ictl_l10n::trf!("没有实例 {name}", "There is no instance {name}"));
        std::fs::remove_file(&pending)?;
        println!("{}", ictl_l10n::trf!("已清掉 {name} 没做完的添加", "Cleared {name}'s unfinished add"));
        return Ok(());
    };
    if !crate::service::instance_stop(name) {
        bail!(
            "{}",
            ictl_l10n::trf!(
                "{name} 的服务没能停下，登记留着",
                "{name}'s services couldn't be stopped, so its registration stays"
            )
        );
    }
    let stopped =
        tokio::time::timeout(Duration::from_secs(2), snapshot(&reg.managed.socket)).await.map_or(true, |r| r.is_err());
    ensure!(
        stopped,
        "{}",
        ictl_l10n::trf!(
            "{name} 的守护进程还在应答（不是服务启动的？），登记留着",
            "{name}'s daemon still answers (not started by its service?), so its registration stays"
        )
    );
    // its background WDAs, as `service stop` ends them: by its id, on the devices whose lease can be taken
    let root = ictl_daemon::Config::from_env(&ictl_daemon::protocol::default_data_dir()).root;
    let mut cfg = ictl_daemon::Config::from_env(&reg.managed.data_dir);
    (cfg.instance, cfg.instance_id, cfg.root) = (Some(name.into()), reg.managed.id.clone(), root);
    for (udid, s) in ictl_daemon::detached::end_recorded(&cfg).await {
        println!("  WDA {udid}: {s:?}");
    }
    let left = instance::scan(&registry).map(|s| s.registered.len() > 1).unwrap_or(true);
    crate::service::instance_remove_files(name, left)?;
    std::fs::remove_file(registry.join(format!("{name}.env")))?;
    let _ = std::fs::remove_file(&pending);
    if purge {
        std::fs::remove_dir_all(&reg.managed.data_dir)
            .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })?;
    }
    println!("{}", ictl_l10n::trf!("已移除实例 {name}", "Removed the instance {name}"));
    Ok(())
}
