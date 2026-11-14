//! Apps: listing, resolving a name to a bundle ID, launching, quitting, opening links

use super::*;

impl Daemon {
    pub(crate) async fn list_apps(&self, g: &mut Inner, include_system: bool) -> Result<Vec<AppInfo>, String> {
        if let Some(a) = g.dev.apps.get(&include_system) {
            return Ok(a.clone());
        }
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let a = device.list_apps(include_system).await.map_err(|e| e.to_string())?;
        g.dev.apps.insert(include_system, a.clone());
        Ok(a)
    }

    pub(crate) async fn resolve_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        if apps::looks_like_bundle_id(app) {
            return Ok(app.to_owned());
        }
        if let Some(b) = self.resolve_cached(g, app).await? {
            return Ok(b);
        }
        // 守护进程启动后新装的 App 不在缓存里：刷新一次再找
        g.dev.apps.clear();
        self.resolve_cached(g, app).await?.ok_or_else(|| {
            ictl_l10n::trf!(
                "找不到名为 \"{app}\" 的 App,用 iphone_list_apps 查看",
                "No app named \"{app}\"; see iphone_list_apps"
            )
        })
    }

    pub(super) async fn resolve_cached(&self, g: &mut Inner, app: &str) -> Result<Option<String>, String> {
        // 先只查用户 App；列系统 App 要好几秒，只在找不到时才去取
        let user = self.list_apps(g, false).await?;
        if let Some(b) = user.iter().find(|a| a.name == app) {
            return Ok(Some(b.bundle_id.clone()));
        }
        let system = self.list_apps(g, true).await?;
        Ok(apps::resolve(app, [&user[..], &system[..]]))
    }

    pub(crate) async fn launch_app(&self, g: &mut Inner, app: &str) -> Result<(String, bool), String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        let w = self.require_wda(g)?;
        let r = w.launch_app(&bid).await.map_err(|e| e.to_string())?;
        let blocked = matches!(r, Launch::Blocked(_));
        let text = match r {
            Launch::Foreground => ictl_l10n::trf!("已启动 {bid}", "Launched {bid}"),
            Launch::Blocked(text) => {
                ictl_l10n::trf!(
                    "已启动 {bid},但{};{TAP_ALERT}",
                    "Launched {bid}, but {}; {TAP_ALERT_EN}",
                    blocking(&text)
                )
            }
            Launch::Slow => ictl_l10n::trf!(
                "已启动 {bid},但 8 秒内没到前台(可能启动慢),先截图确认",
                "Launched {bid}, but it didn't come to the foreground within 8 s (maybe slow to start); check with a screenshot"
            ),
        };
        Ok((text, blocked))
    }

    pub(crate) async fn terminate_app(&self, g: &mut Inner, app: &str) -> Result<String, String> {
        let bid = self.resolve_app(g, app).await?;
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.terminate_app(&bid).await.map_err(|e| e.to_string())?;
        Ok(ictl_l10n::trf!("已结束 {bid}", "Terminated {bid}"))
    }

    pub(crate) async fn open_url(&self, g: &mut Inner, url: &str) -> Result<(String, bool), String> {
        self.ensure_unlocked(g).await?;
        let (r, app) = self.require_wda(g)?.open_url(url).await.map_err(|e| e.to_string())?;
        let blocked = matches!(r, Launch::Blocked(_));
        let text = match r {
            Launch::Foreground => ictl_l10n::trf!("已打开 {url}(前台 {app})", "Opened {url} (foreground {app})"),
            Launch::Blocked(text) => {
                ictl_l10n::trf!(
                    "已打开 {url},但{};{TAP_ALERT}",
                    "Opened {url}, but {}; {TAP_ALERT_EN}",
                    blocking(&text)
                )
            }
            Launch::Slow => ictl_l10n::trf!(
                "已打开 {url},但 8 秒内没有 App 到前台(前台还是 {app}),先截图确认",
                "Opened {url}, but no app came to the foreground within 8 s (still {app}); check with a screenshot"
            ),
        };
        Ok((text, blocked))
    }
}
