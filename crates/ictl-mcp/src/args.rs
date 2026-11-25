use super::*;

pub(crate) struct Args<'a>(pub(crate) &'a Map<String, Value>);

/// 数字，或写成字符串的数字（toolkit 的 pydantic 也接受 "3"）
pub(crate) fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok()).filter(|x: &f64| x.is_finite())
}

/// 参数给了但类型不对就报错，不再悄悄用默认值（例如 index: -1 原来会点第 0 个）
impl Args<'_> {
    pub(crate) fn get(&self, k: &str) -> Option<&Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }
    pub(crate) fn f(&self, k: &str) -> Result<f64, String> {
        self.opt_f(k)?.ok_or_else(|| ictl_l10n::trf!("缺少数字参数 {k}", "missing number parameter {k}"))
    }
    pub(crate) fn f_or(&self, k: &str, d: f64) -> Result<f64, String> {
        Ok(self.opt_f(k)?.unwrap_or(d))
    }
    pub(crate) fn opt_f(&self, k: &str) -> Result<Option<f64>, String> {
        self.get(k)
            .map(|v| number(v).ok_or_else(|| ictl_l10n::trf!("参数 {k} 要是数字", "parameter {k} must be a number")))
            .transpose()
    }
    pub(crate) fn s(&self, k: &str) -> Result<String, String> {
        self.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ictl_l10n::trf!("缺少文字参数 {k}", "missing text parameter {k}"))
    }
    pub(crate) fn s_or(&self, k: &str, d: &str) -> Result<String, String> {
        match self.get(k) {
            None => Ok(d.to_owned()),
            Some(v) => v
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ictl_l10n::trf!("参数 {k} 要是文字", "parameter {k} must be text")),
        }
    }
    pub(crate) fn u_or(&self, k: &str, d: usize) -> Result<usize, String> {
        match self.get(k) {
            None => Ok(d),
            Some(v) => number(v).filter(|x| *x >= 0.0 && x.fract() == 0.0).map(|x| x as usize).ok_or_else(|| {
                ictl_l10n::trf!("参数 {k} 要是非负整数", "parameter {k} must be a non-negative integer")
            }),
        }
    }
    /// 连点次数：不给是 1，给了要是 1 到 [`MAX_TAP_COUNT`] 的整数
    pub(crate) fn count(&self, k: &str) -> Result<u32, String> {
        match self.u_or(k, 1)? {
            n @ 1.. if n <= MAX_TAP_COUNT as usize => Ok(n as u32),
            _ => Err(ictl_l10n::trf!(
                "参数 {k} 要是 1 到 {MAX_TAP_COUNT} 的整数",
                "parameter {k} must be a whole number from 1 to {MAX_TAP_COUNT}"
            )),
        }
    }
    pub(crate) fn b_or(&self, k: &str, d: bool) -> Result<bool, String> {
        match self.get(k) {
            None => Ok(d),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(true),
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(false),
            Some(_) => Err(ictl_l10n::trf!("参数 {k} 要是 true 或 false", "parameter {k} must be true or false")),
        }
    }
}

/// 工具调用 → 守护进程请求
/// `iphone_tap_element` 的 `scroll` / `max_scrolls`：没给 scroll 时不滑
pub(crate) fn scroll_search(a: &Args) -> Result<Option<ictl_protocol::ScrollSearch>, String> {
    let dir = a.s_or("scroll", "")?;
    let max = a.u_or("max_scrolls", 8)?;
    if dir.is_empty() {
        return Ok(None);
    }
    checked_scroll(&dir, max).map(Some)
}

pub(crate) fn checked_scroll(dir: &str, max: usize) -> Result<protocol::ScrollSearch, String> {
    let down = match dir {
        "down" => true,
        "up" => false,
        _ => return Err(ictl_l10n::trf!("参数 scroll 要是 down 或 up", "parameter scroll must be down or up")),
    };
    if max == 0 || max > ictl_protocol::MAX_SEARCH_SCROLLS as usize {
        return Err(ictl_l10n::trf!(
            "参数 max_scrolls 要在 1 到 {} 之间",
            "parameter max_scrolls must be between 1 and {}",
            ictl_protocol::MAX_SEARCH_SCROLLS
        ));
    }
    Ok(protocol::ScrollSearch { down, max: max as u32 })
}

pub fn to_request(name: &str, args: &Map<String, Value>) -> Result<Request, String> {
    let a = Args(args);
    Ok(match name {
        "iphone_status" => Request::Status,
        "iphone_screenshot" => {
            let r = [a.opt_f("x")?, a.opt_f("y")?, a.opt_f("width")?, a.opt_f("height")?];
            let region = match r {
                [None, None, None, None] => None,
                [Some(x), Some(y), Some(w), Some(h)] => Some((x, y, w, h)),
                _ => {
                    return Err(ictl_l10n::tr!(
                        "放大区域要同时给出 x、y、width、height",
                        "a zoom region needs all of x, y, width and height"
                    )
                    .into());
                }
            };
            Request::Screenshot { region }
        }
        "iphone_list_elements" => Request::ListElements {
            filter: a.s_or("filter", "")?,
            limit: a.u_or("limit", 150)?,
            max_age_ms: 0,
            keep_locked: false,
        },
        "iphone_scroll_collect" => {
            let limit = a.u_or("limit", protocol::MAX_SCROLL_ROWS)?;
            if !(1..=protocol::MAX_SCROLL_ROWS).contains(&limit) {
                return Err(ictl_l10n::trf!(
                    "limit 要在 1 到 {} 之间",
                    "limit must be between 1 and {}",
                    protocol::MAX_SCROLL_ROWS
                ));
            }
            let scroll = checked_scroll(&a.s_or("scroll", "down")?, a.u_or("max_scrolls", 8)?)?;
            let x = a.opt_f("x")?;
            if x.is_some_and(|x| !x.is_finite() || x < 0.0) {
                return Err(ictl_l10n::tr!("x 不能是负数", "x can't be negative").into());
            }
            Request::ScrollCollect(protocol::ScrollCollect {
                down: scroll.down,
                max_scrolls: scroll.max,
                until: a.s_or("until", "")?,
                filter: a.s_or("filter", "")?,
                limit,
                x,
            })
        }
        "iphone_tap" => Request::Tap { x: a.f("x")?, y: a.f("y")?, count: a.count("count")? },
        "iphone_tap_element" => Request::TapElement {
            text: a.s("text")?,
            index: a.u_or("index", 0)?,
            max_age_ms: 0,
            scroll: scroll_search(&a)?,
        },
        "iphone_long_press" => Request::LongPress { x: a.f("x")?, y: a.f("y")?, seconds: a.f_or("seconds", 1.0)? },
        "iphone_swipe" => Request::Swipe {
            from: (a.f("from_x")?, a.f("from_y")?),
            to: (a.f("to_x")?, a.f("to_y")?),
            seconds: a.f_or("seconds", 0.3)?,
            hold: a.f_or("hold", 0.0)?,
        },
        "iphone_type_text" => {
            let text = a.s("text")?;
            let submit = a.b_or("submit", false)?;
            if a.b_or("replace", false)? {
                Request::ReplaceText { text, submit }
            } else {
                Request::TypeText { text, submit }
            }
        }
        "iphone_press_button" => Request::PressButton { button: a.s("button")? },
        "iphone_list_apps" => Request::ListApps { include_system: a.b_or("include_system", false)? },
        "iphone_launch_app" => Request::LaunchApp { app: a.s("app")? },
        "iphone_terminate_app" => Request::TerminateApp { app: a.s("app")? },
        "iphone_open_url" => Request::OpenUrl { url: a.s("url")? },
        "iphone_list_devices" => Request::ListDevices,
        "iphone_select_device" => Request::SelectDevice { device: a.s("device")? },
        "iphone_app_logs" => Request::AppLogs {
            process: a.s_or("process", "")?,
            grep: a.s_or("grep", "")?,
            since_secs: a.u_or("since", 60)? as u64,
            limit: a.u_or("limit", 100)?,
            errors_only: a.b_or("errors_only", false)?,
        },
        "iphone_crash_reports" => Request::CrashReports { filter: a.s_or("filter", "")?, name: a.s_or("name", "")? },
        "iphone_install_app" => Request::InstallApp { path: a.s("path")? },
        "iphone_uninstall_app" => Request::UninstallApp { app: a.s("app")? },
        "iphone_web_targets" => Request::WebTargets,
        "iphone_web_eval" => {
            let secs = a.f_or("timeout", 10.0)?;
            if !(0.0..=60.0).contains(&secs) {
                return Err(
                    ictl_l10n::tr!("timeout 要在 0 到 60 秒之间", "timeout must be between 0 and 60 seconds").into()
                );
            }
            Request::WebEval {
                target: a.s_or("target", "")?,
                expression: a.s("expression")?,
                timeout_ms: (secs * 1000.0) as u64,
            }
        }
        "iphone_wait_for" => {
            let secs = a.f_or("timeout", 5.0)?;
            if !(0.0..=30.0).contains(&secs) {
                return Err(
                    ictl_l10n::tr!("timeout 要在 0 到 30 秒之间", "timeout must be between 0 and 30 seconds").into()
                );
            }
            let text = a.s("text")?;
            let timeout_ms = (secs * 1000.0) as u64;
            match a.s_or("state", "appeared")?.as_str() {
                "appeared" => Request::WaitFor { text, timeout_ms },
                "disappeared" => Request::WaitForDisappearance { text, timeout_ms },
                _ => {
                    return Err(ictl_l10n::tr!(
                        "state 只能是 appeared 或 disappeared",
                        "state must be appeared or disappeared"
                    )
                    .into());
                }
            }
        }
        other => return Err(ictl_l10n::trf!("未知工具 {other}", "unknown tool {other}")),
    })
}

/// 操作后顺带返回什么
#[derive(Debug, Clone, PartialEq)]
pub enum Then {
    None,
    Elements(String),
    Screenshot(Option<(f64, f64, f64, f64)>),
}

/// then 带的选项：返回什么 + 最多等多久让界面稳定
#[derive(Debug, Clone, PartialEq)]
pub struct ThenOpts {
    pub then: Then,
    pub settle_ms: u64,
}

pub fn parse_then(args: &Map<String, Value>) -> Result<ThenOpts, String> {
    let a = Args(args);
    let then = match a.s_or("then", "none")?.as_str() {
        "none" | "" => Then::None,
        "elements" => Then::Elements(a.s_or("then_filter", "")?),
        "screenshot" => {
            let region =
                match a.get("then_region") {
                    None => None,
                    Some(v) => {
                        let xs: Option<Vec<f64>> = v.as_array().map(|arr| arr.iter().filter_map(number).collect());
                        match xs.as_deref() {
                            Some([x, y, w, h]) if *w > 0.0 && *h > 0.0 => Some((*x, *y, *w, *h)),
                            _ => return Err(ictl_l10n::tr!(
                                "then_region 要是 [x, y, width, height] 四个数,宽高大于 0",
                                "then_region must be four numbers [x, y, width, height], with width and height above 0"
                            )
                            .into()),
                        }
                    }
                };
            Then::Screenshot(region)
        }
        other => {
            return Err(ictl_l10n::trf!(
                "then 只能是 none / elements / screenshot,不是 {other}",
                "then must be none / elements / screenshot, not {other}"
            ));
        }
    };
    let secs = a.f_or("then_settle", THEN_SETTLE_MS as f64 / 1000.0)?;
    if !(0.0..=10.0).contains(&secs) {
        return Err(
            ictl_l10n::tr!("then_settle 要在 0 到 10 秒之间", "then_settle must be between 0 and 10 seconds").into()
        );
    }
    Ok(ThenOpts { then, settle_ms: (secs * 1000.0) as u64 })
}

/// Each tool's argument names: the properties of its input schema, the developer tools' and
/// [`task::CONFIRM_TOOL`]'s included
pub(crate) static ARG_NAMES: std::sync::LazyLock<std::collections::HashMap<String, Vec<String>>> =
    std::sync::LazyLock::new(|| {
        tools_with(true)
            .into_iter()
            .chain([confirm_tool()])
            .map(|t| {
                let names = t
                    .input_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|p| p.keys().cloned().collect())
                    .unwrap_or_default();
                (t.name.to_string(), names)
            })
            .collect()
    });

/// Refuses an argument `tool` doesn't take, before anything is done. Unknown arguments used to be dropped: a call
/// with a misspelt name, or with one this version doesn't have yet, ran with the default instead and reported success
/// (`iphone_wait_for` with `state: "disappeared"` answered “appeared” at once, #158). `extra`: names taken besides the
/// schema's (a batch step's `tool`); `then` and the like are only taken where the schema lists them, so not in a
/// batch's steps (the batch's own then applies). A tool this server doesn't know is left to the call to refuse
pub fn check_args(tool: &str, args: &Map<String, Value>, extra: &[&str], then: bool) -> Result<(), String> {
    let Some(names) = ARG_NAMES.get(tool) else { return Ok(()) };
    let then_names = ["then", "then_filter", "then_region", "then_settle"];
    let takes = |k: &str| (names.iter().any(|n| n == k) && (then || !then_names.contains(&k))) || extra.contains(&k);
    let mut unknown: Vec<&str> = args.keys().map(String::as_str).filter(|k| !takes(k)).collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    let mut known: Vec<&str> =
        names.iter().map(String::as_str).filter(|k| takes(k)).chain(extra.iter().copied()).collect();
    known.sort_unstable();
    known.dedup();
    let sep = ictl_l10n::tr!("、", ", ");
    let known = if known.is_empty() { ictl_l10n::tr!("(无)", "(none)").to_owned() } else { known.join(sep) };
    Err(ictl_l10n::trf!(
        "{tool} 没有参数 {},什么都没做;它的参数:{known}",
        "{tool} has no argument {}, nothing was done; its arguments: {known}",
        unknown.join(sep)
    ))
}
