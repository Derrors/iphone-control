//! `iphone-control task`: AI tasks from the command line, for agent platforms other than the control page
//! (docs/task-control.md).
//!
//! `task run -- <command>` is the whole integration for most platforms: it opens a task on the controlled phone,
//! holds its lease, and runs the command with the task's token in `IPHONE_CONTROL_TASK_TOKEN`, so an
//! `iphone-control mcp` started anywhere below it takes part in the task (the page shows its steps, pausing and
//! taking over hold it, confirmations wait for the user). When the command exits the task ends with its exit
//! status; when the task ends elsewhere (stopped on the page, the phone changed), the command is stopped. Ctrl-C
//! stops the task. The other subcommands follow and steer tasks, whoever started them.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Subcommand;
use ictl_daemon::protocol::{self, Request, Response};

#[derive(Subcommand)]
pub enum TaskAction {
    #[command(about = ictl_l10n::tr!("开一个任务并在其中运行命令：命令里启动的 iphone-control mcp 自动接入这个任务（控制台能看到每一步、暂停、接管、确认）。命令结束任务就结束；任务在别处被停止时命令也被停止", "Open a task and run a command in it: an iphone-control mcp started by the command takes part in the task (the control page shows every step and can pause, take over and confirm). The task ends when the command does; when the task is stopped elsewhere, so is the command"))]
    Run {
        #[arg(long, help = ictl_l10n::tr!("任务标题，显示在控制台上；默认是命令本身", "The task's title, shown on the control page; the command itself by default"))]
        title: Option<String>,
        #[arg(long, default_value = "external", help = ictl_l10n::tr!("谁在执行，显示在控制台上", "Who runs it, shown on the control page"))]
        owner: String,
        #[arg(long = "app", value_name = "APP", help = ictl_l10n::tr!("任务只在这些 App 里操作（App 名或 Bundle ID，可以写多次）；到了别的 App 每一步都先等你确认。不写就不限制", "Limit the task to these apps (name or bundle ID, repeatable); in any other app each step waits for you first. No limit if not given"))]
        apps: Vec<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(about = ictl_l10n::tr!("最近的任务（--json 给程序读）", "Recent tasks (--json for programs)"))]
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(about = ictl_l10n::tr!("跟着一个任务的事件，每行一个 JSON，任务结束时退出", "Follow a task's events, one JSON per line, until it ends"))]
    Events { id: String },
    #[command(about = ictl_l10n::tr!("暂停：任务停在下一步之前", "Pause: the task holds before its next step"))]
    Pause { id: String },
    #[command(about = ictl_l10n::tr!("继续暂停的任务", "Resume a paused task"))]
    Resume { id: String },
    #[command(about = ictl_l10n::tr!("接管：任务停在下一步之前，你可以自己操作手机", "Take over: the task holds before its next step and you may act on the phone"))]
    Takeover { id: String },
    #[command(about = ictl_l10n::tr!("交还控制：任务先重新看界面再继续", "Hand back: the task looks at the screen again, then goes on"))]
    Handback { id: String },
    #[command(about = ictl_l10n::tr!("停止任务（task run 的命令也会被停止）", "Stop the task (and the command of a task run)"))]
    Stop { id: String },
    #[command(about = ictl_l10n::tr!("同意任务正在等确认的那一步", "Approve the step the task is waiting on"))]
    Approve {
        id: String,
        #[arg(long)]
        step: Option<u64>,
    },
    #[command(about = ictl_l10n::tr!("拒绝任务正在等确认的那一步", "Reject the step the task is waiting on"))]
    Reject {
        id: String,
        #[arg(long)]
        step: Option<u64>,
    },
    #[command(about = ictl_l10n::tr!("在任务里（IPHONE_CONTROL_TASK_TOKEN）给出结果并结束任务；task run 的命令可以用它写总结", "From inside a task (IPHONE_CONTROL_TASK_TOKEN): end it with a result; a command under task run can use it for its summary"))]
    Finish {
        #[arg(long)]
        failed: bool,
        summary: Vec<String>,
    },
}

/// Commands that print and end (see `restore_sigpipe` in main)
pub fn prints_and_exits(a: &TaskAction) -> bool {
    matches!(a, TaskAction::List { .. } | TaskAction::Events { .. })
}

pub async fn run(socket: &Path, action: TaskAction) -> anyhow::Result<()> {
    let control = |id: String, action: &str, step: Option<u64>, approve: bool| Request::TaskControl {
        id,
        action: action.into(),
        step,
        approve,
        by: Some("cli".into()),
        for_pid: None,
    };
    let req = match action {
        TaskAction::Run { title, owner, apps, command } => {
            return run_command(socket, title, &owner, &apps, &command).await;
        }
        TaskAction::List { json } => return list(socket, json).await,
        TaskAction::Events { id } => {
            let mut s = protocol::task_events(socket, &id).await.map_err(anyhow::Error::msg)?;
            while let Some(e) = s.next().await {
                println!("{}", serde_json::to_string(&e)?);
            }
            return Ok(());
        }
        TaskAction::Finish { failed, summary } => {
            let token = std::env::var(protocol::TASK_TOKEN_ENV).ok().filter(|t| !t.is_empty()).with_context(|| {
                ictl_l10n::trf!("没有设 {}：不在任务里", "{} isn't set: not inside a task", protocol::TASK_TOKEN_ENV)
            })?;
            let req = Request::TaskFinish { ok: !failed, summary: summary.join(" ") };
            return expect_ok(protocol::as_task(Some(token), None, ask(socket, &req)).await);
        }
        TaskAction::Pause { id } => control(id, "pause", None, false),
        TaskAction::Resume { id } => control(id, "resume", None, false),
        TaskAction::Takeover { id } => control(id, "takeover", None, false),
        TaskAction::Handback { id } => control(id, "handback", None, false),
        TaskAction::Stop { id } => control(id, "stop", None, false),
        TaskAction::Approve { id, step } => control(id, "confirm", step, true),
        TaskAction::Reject { id, step } => control(id, "confirm", step, false),
    };
    // inside a task (its agent's shell) the token goes along, and the daemon refuses: only the user controls a task
    let token = std::env::var(protocol::TASK_TOKEN_ENV).ok().filter(|t| !t.is_empty());
    expect_ok(protocol::as_task(token, None, ask(socket, &req)).await)
}

async fn ask(socket: &Path, req: &Request) -> anyhow::Result<Response> {
    protocol::call(socket, req, Some(Duration::from_secs(10))).await.with_context(|| {
        ictl_l10n::trf!(
            "连不上守护进程（{}）；确认它在运行（iphone-control status）",
            "Can't reach the daemon ({}); make sure it is running (iphone-control status)",
            socket.display()
        )
    })
}

fn expect_ok(r: anyhow::Result<Response>) -> anyhow::Result<()> {
    match r? {
        Response::Error { message, .. } => bail!("{message}"),
        _ => Ok(()),
    }
}

async fn list(socket: &Path, json: bool) -> anyhow::Result<()> {
    let tasks = protocol::task_list(socket).await.map_err(anyhow::Error::msg)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&tasks)?);
        return Ok(());
    }
    if tasks.is_empty() {
        println!("{}", ictl_l10n::tr!("还没有任务", "No tasks yet"));
    }
    for t in &tasks {
        let title: String = t.title.lines().next().unwrap_or_default().chars().take(60).collect();
        println!("{}  {:<11} {:>3} {}  {:<10} {title}", t.id, t.state, t.steps, ictl_l10n::tr!("步", "steps"), t.owner);
    }
    Ok(())
}

/// `task run`: the task lives as long as the command
async fn run_command(
    socket: &Path,
    title: Option<String>,
    owner: &str,
    apps: &[String],
    command: &[String],
) -> anyhow::Result<()> {
    let title = title.unwrap_or_else(|| command.join(" "));
    let (opened, mut lease) = protocol::open_task(socket, &title, owner, apps).await.map_err(anyhow::Error::msg)?;
    eprintln!("{}", ictl_l10n::trf!("任务 {} 已开始", "Task {} started", opened.id));
    if !apps.is_empty() {
        eprintln!("{}", ictl_l10n::trf!("只在这些 App 里操作：{}", "Limited to these apps: {}", apps.join(", ")));
    }
    // the command and what it starts may not control the task, only stop it; said before it starts
    let _ = protocol::as_task(Some(opened.token.clone()), None, ask(socket, &Request::TaskAgent)).await;
    let spawned = tokio::process::Command::new(&command[0])
        .args(&command[1..])
        .env(protocol::TASK_TOKEN_ENV, &opened.token)
        .env("IPHONE_CONTROL_TASK_ID", &opened.id)
        // its own group: stopping the task stops everything it started (an agent and its MCP server)
        .process_group(0)
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let why = ictl_l10n::trf!("启动不了 {}：{e}", "Can't start {}: {e}", command[0]);
            // ended as failed while the lease is still held (dropping it first would make it abandoned)
            let req = Request::TaskFinish { ok: false, summary: why.clone() };
            let _ = protocol::as_task(Some(opened.token.clone()), None, ask(socket, &req)).await;
            drop(lease);
            bail!("{why}");
        }
    };
    let pid = child.id().map(|p| p as libc::pid_t);
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut outcome: Option<String> = None;
    let mut ended = false;
    let status = loop {
        tokio::select! {
            s = child.wait() => break s?,
            e = lease.next(), if !ended => {
                if let Some(e) = &e && e.event != "done" {
                    continue;
                }
                // ended elsewhere, or the daemon went away: the command can't go on
                ended = true;
                outcome = e.and_then(|e| e.data["outcome"].as_str().map(str::to_owned));
                stop_group(pid);
            }
            _ = tokio::signal::ctrl_c() => stop_here(socket, &opened.id, pid).await,
            _ = term.recv() => stop_here(socket, &opened.id, pid).await,
        }
    };
    if !ended {
        // the command may have ended the task itself (`task finish`); then this changes nothing
        let req = Request::TaskFinish { ok: status.success(), summary: String::new() };
        let _ = protocol::as_task(Some(opened.token.clone()), None, ask(socket, &req)).await;
    }
    let outcome = match outcome {
        Some(o) => o,
        None => protocol::task_list(socket)
            .await
            .ok()
            .and_then(|l| l.into_iter().find(|t| t.id == opened.id))
            .map(|t| t.state)
            .unwrap_or_default(),
    };
    eprintln!("{}", ictl_l10n::trf!("任务 {} 结束：{outcome}", "Task {} ended: {outcome}", opened.id));
    drop(lease);
    std::process::exit(status.code().unwrap_or(1));
}

/// Ctrl-C or SIGTERM: the user stops the task (the lease stream then reports it, and the command is stopped)
async fn stop_here(socket: &Path, id: &str, pid: Option<libc::pid_t>) {
    let req = Request::TaskControl {
        id: id.into(),
        action: "stop".into(),
        step: None,
        approve: false,
        by: Some("owner".into()),
        for_pid: None,
    };
    if !matches!(ask(socket, &req).await, Ok(Response::Text { .. })) {
        stop_group(pid);
    }
}

/// SIGTERM to the command's process group, SIGKILL a little later if still there
fn stop_group(pid: Option<libc::pid_t>) {
    let Some(pgid) = pid else { return };
    unsafe { libc::kill(-pgid, libc::SIGTERM) };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    });
}
