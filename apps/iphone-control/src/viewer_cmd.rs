//! `iphone-control viewer token …` subcommand handler.

use std::path::Path;

use crate::{TokenAction, viewer};

/// `s` padded to `width` columns, CJK and other wide characters taking two
fn pad(s: &str, width: usize) -> String {
    let w: usize =
        s.chars().map(|c| if ('\u{1100}'..='\u{ffdc}').contains(&c) && c > '\u{2e7f}' { 2 } else { 1 }).sum();
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

/// Where the viewer listens, for links that log in
pub(crate) struct ViewerAt<'a> {
    pub(crate) port: u16,
    pub(crate) lan: bool,
    pub(crate) lan_hosts: &'a [String],
    pub(crate) tls: &'a viewer::tls::Setting,
    pub(crate) mcp: bool,
}

/// `viewer token …`: the viewer's login tokens. The viewer reads the file again when it changes
pub(crate) fn viewer_token(
    data_dir: &Path,
    at: &ViewerAt,
    action: Option<TokenAction>,
    new: bool,
) -> anyhow::Result<()> {
    use viewer::auth;
    let entry = match action {
        None if new => auth::renew(data_dir, auth::OWNER)?,
        None => auth::get(data_dir, auth::OWNER)?,
        Some(TokenAction::Show { name, new: true }) => auth::renew(data_dir, &name)?,
        Some(TokenAction::Show { name, new: false }) => auth::get(data_dir, &name)?,
        Some(TokenAction::Add { name, read_only }) => {
            auth::add(data_dir, &name, if read_only { auth::Role::View } else { auth::Role::Operate })?
        }
        Some(TokenAction::List) => {
            for e in auth::load(data_dir)? {
                let role = match e.role {
                    auth::Role::Operate => ictl_l10n::tr!("可操作", "operate"),
                    auth::Role::View => ictl_l10n::tr!("只读", "read-only"),
                };
                let created = chrono::DateTime::from_timestamp(e.created as i64, 0)
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_default();
                println!("{} {} {created}", pad(&e.name, 24), pad(role, 10));
            }
            println!("{}", ictl_l10n::trf!("（存在 {}）", "(kept in {})", auth::tokens_file(data_dir).display()));
            return Ok(());
        }
        Some(TokenAction::Remove { name }) => {
            auth::remove(data_dir, &name)?;
            println!(
                "{}",
                ictl_l10n::trf!(
                    "已删掉 {name}：用它登录的浏览器和程序马上进不来了。",
                    "Removed {name}: browsers and programs logged in with it are out now."
                )
            );
            return Ok(());
        }
    };
    let (token, port) = (&entry.token, at.port);
    let role = match entry.role {
        auth::Role::Operate => ictl_l10n::tr!("可操作", "may operate"),
        auth::Role::View => ictl_l10n::tr!(
            "只读：只能看画面、设备状态和任务",
            "read-only: watches the picture, the device's state and tasks"
        ),
    };
    println!("{}", ictl_l10n::trf!("令牌    {}（{role}）", "Token   {} ({role})", entry.name));
    println!("        {token}");
    println!(
        "{}",
        ictl_l10n::trf!(
            "本机    http://127.0.0.1:{port}/?token={token}",
            "Here    http://127.0.0.1:{port}/?token={token}"
        )
    );
    let scheme = if at.tls.enabled(at.lan) { "https" } else { "http" };
    if at.lan {
        for a in viewer::lan_addresses(at.lan_hosts) {
            println!(
                "{}",
                ictl_l10n::trf!(
                    "局域网  {scheme}://{a}:{port}/?token={token}",
                    "Network {scheme}://{a}:{port}/?token={token}"
                )
            );
        }
    }
    if at.mcp && entry.role == auth::Role::Operate {
        println!(
            "MCP     claude mcp add --transport http iphone http://127.0.0.1:{port}/mcp --header \"Authorization: Bearer {token}\""
        );
    }
    println!(
        "{}",
        ictl_l10n::tr!(
            "链接里带着令牌：别发到聊天或公开的地方。打开一次后浏览器会记住登录（30 天），地址栏里的令牌会被去掉。",
            "The links carry the token: don't paste them into chats or anywhere public. Opened once, the browser stays logged in (30 days) and the token leaves the address bar."
        )
    );
    if new {
        println!(
            "{}",
            ictl_l10n::tr!(
                "已换成新令牌，马上生效：用旧令牌登录的浏览器和 MCP 客户端都要用新的。",
                "The token was replaced, effective now: browsers and MCP clients that used the old one need the new one."
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn token_names_line_up_in_columns() {
        assert_eq!(super::pad("owner", 8), "owner   ");
        assert_eq!(super::pad("客厅电视", 10), "客厅电视  ", "four wide characters are eight columns");
        assert_eq!(super::pad("toolongname", 4), "toolongname");
    }
}
