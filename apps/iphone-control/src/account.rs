//! Interactive Apple ID sign-in, device passcode management, and TTY input helpers.

use std::io::Write;
use std::path::Path;

use anyhow::bail;

use crate::PasscodeAction;
use crate::client::{account_dir, target_device};

pub(crate) fn read_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    // Do not use Stdin's global BufReader here: it can read ahead into the
    // next menu's pasted answer, which the raw terminal reader cannot see.
    use std::io::Read;
    use std::os::fd::AsFd;
    let mut input = std::fs::File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    let mut bytes = Vec::new();
    let mut byte = [0];
    let mut overflow = false;
    loop {
        match input.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                if bytes.len() < 4096 {
                    bytes.push(byte[0]);
                } else {
                    overflow = true;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::ensure!(!overflow, "{}", ictl_l10n::tr!("输入超过 4096 字节", "Input exceeds 4096 bytes"));
    if byte[0] != b'\n' {
        bail!(
            "{}",
            ictl_l10n::tr!(
                "读不到输入（标准输入已关闭）：在终端里运行",
                "Can't read input (stdin is closed): run it in a terminal"
            )
        );
    }
    Ok(String::from_utf8(bytes)?.trim().to_owned())
}

/// 不回显地读一行（用 stty 关回显，不引入额外依赖）
pub(crate) fn read_secret(prompt: &str) -> anyhow::Result<String> {
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

pub(crate) async fn login(data_dir: &Path, email: Option<String>) -> anyhow::Result<()> {
    println!(
        "{}",
        ictl_l10n::tr!(
            "建议用一个专门的免费 Apple ID，不要用主账号。",
            "Use a dedicated free Apple ID, not your main account."
        )
    );
    println!(
        "{}",
        ictl_l10n::tr!(
            "密码和验证码由本程序直接发给苹果；登录时会请求 anisette 服务生成设备标识（默认第三方，可用 ICTL_ANISETTE_URL 改成自建）。",
            "The password and verification code go straight from this program to Apple; signing in asks an anisette service for device identifiers (a third-party one by default, set ICTL_ANISETTE_URL to use your own)."
        )
    );
    let email = match email {
        Some(e) => e,
        None => read_line(ictl_l10n::tr!("Apple ID 邮箱: ", "Apple ID email: "))?,
    };
    if email.is_empty() {
        bail!("{}", ictl_l10n::tr!("没有输入 Apple ID 邮箱", "No Apple ID email entered"));
    }
    let password = read_secret(ictl_l10n::tr!("密码(输入不显示): ", "Password (hidden): "))?;
    if password.is_empty() {
        bail!("{}", ictl_l10n::tr!("没有输入密码", "No password entered"));
    }
    let two_factor = |req: ictl_sign::TwoFactorRequest| -> Result<ictl_sign::TwoFactorAction, String> {
        let sms = !req.trusted_phone_numbers.is_empty();
        let prompt = match req.method {
            ictl_sign::TwoFactorMethod::Sms => ictl_l10n::tr!("短信验证码: ", "SMS code: "),
            ictl_sign::TwoFactorMethod::Device if sms => ictl_l10n::tr!(
                "设备上收到的验证码(输入 sms 改为短信接收): ",
                "Code shown on your device (enter sms to get it by text message): "
            ),
            ictl_sign::TwoFactorMethod::Device => ictl_l10n::tr!("设备上收到的验证码: ", "Code shown on your device: "),
        };
        let code = read_line(prompt).map_err(|e| e.to_string())?;
        if sms && code.eq_ignore_ascii_case("sms") {
            return Ok(ictl_sign::TwoFactorAction::SendSms(req.trusted_phone_numbers[0].id));
        }
        Ok(ictl_sign::TwoFactorAction::SubmitCode(code))
    };
    account_dir(data_dir).login(&email, &password, two_factor).await?;
    println!(
        "{}",
        ictl_l10n::trf!(
            "登录成功，会话已保存到 {}",
            "Signed in, session saved to {}",
            account_dir(data_dir).dir.display()
        )
    );
    Ok(())
}

pub(crate) async fn passcode(data_dir: &Path, socket: &Path, action: PasscodeAction) -> anyhow::Result<()> {
    let cfg = ictl_daemon::Config::from_env(data_dir);
    match action {
        PasscodeAction::Clear { all: true, .. } => {
            let _ = std::fs::remove_dir_all(cfg.passcodes_dir());
            let _ = std::fs::remove_file(cfg.legacy_passcode_file());
            println!(
                "{}",
                ictl_l10n::tr!(
                    "已删除所有设备的锁屏密码，自动解锁关闭。",
                    "Deleted the passcodes of all devices, auto-unlock is off."
                )
            );
        }
        PasscodeAction::Clear { device, .. } => {
            let d = target_device(socket, device.as_deref()).await?;
            let _ = std::fs::remove_file(cfg.passcode_file_for(&d.udid));
            println!(
                "{}",
                ictl_l10n::trf!(
                    "已删除 {} 的锁屏密码，这台的自动解锁关闭。",
                    "Deleted the passcode of {}, auto-unlock is off for it.",
                    ictl_daemon::core::device_label(&d)
                )
            );
        }
        PasscodeAction::Set { device } => {
            let d = target_device(socket, device.as_deref()).await?;
            println!("{}", ictl_l10n::trf!("设备：{}", "Device: {}", ictl_daemon::core::device_label(&d)));
            println!(
                "{}",
                ictl_l10n::tr!(
                    "自动解锁：锁屏后，MCP 操作前会用这个密码解锁手机。",
                    "Auto-unlock: when the phone is locked, MCP actions use this passcode to unlock it first."
                )
            );
            println!(
                "{}",
                ictl_l10n::tr!(
                    "风险：能登录这台机器、或能调用这个 MCP 的人，都能解锁你的手机。",
                    "Risk: anyone who can log in to this machine or call this MCP server can unlock your phone."
                )
            );
            println!(
                "{}",
                ictl_l10n::tr!(
                    "保护：每按一位都核对；整串输完仍锁着就停用自动解锁并推送通知，不会累计输错。只支持纯数字。",
                    "Safeguards: every digit is checked; if the phone is still locked after the whole passcode, auto-unlock is disabled and a notification is sent, so wrong attempts don't pile up. Numeric passcodes only."
                )
            );
            let p = read_secret(ictl_l10n::tr!("锁屏密码(输入不显示): ", "Passcode (hidden): "))?;
            if p.len() < 4 || !p.bytes().all(|b| b.is_ascii_digit()) {
                bail!(
                    "{}",
                    ictl_l10n::tr!(
                        "只支持 4 位以上的纯数字密码",
                        "Only numeric passcodes with 4 or more digits are supported"
                    )
                );
            }
            cfg.write_passcode(&d.udid, &p)?;
            println!(
                "{}",
                ictl_l10n::tr!(
                    "已保存（只用于这台设备）。别忘了在手机上把「自动锁定」从「永不」改回较短时间。",
                    "Saved (for this device only). Remember to set Auto-Lock on the phone back from “Never” to a shorter time."
                )
            );
        }
    }
    Ok(())
}
