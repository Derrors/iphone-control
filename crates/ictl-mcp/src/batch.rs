use super::*;

/// 批量里的一步
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Request { tool: String, req: Request },
    Wait(std::time::Duration),
}

/// 检查并转换批量步骤（一步都不执行就先把参数错误全挡掉）
pub fn batch_steps(args: &Map<String, Value>) -> Result<Vec<Step>, String> {
    let steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or(ictl_l10n::tr!("缺少 steps(数组)", "missing steps (array)"))?;
    let reuse = Args(args).b_or("reuse_elements", false)?;
    let one_gesture = Args(args).b_or("one_gesture", false)?;
    if steps.is_empty() || steps.len() > BATCH_MAX_STEPS {
        return Err(ictl_l10n::trf!(
            "steps 要有 1 到 {BATCH_MAX_STEPS} 步",
            "steps must have 1 to {BATCH_MAX_STEPS} steps"
        ));
    }
    let steps: Vec<Step> = steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let n = i + 1;
            let obj = s.as_object().ok_or_else(|| ictl_l10n::trf!("第 {n} 步不是对象", "step {n} is not an object"))?;
            let tool = obj
                .get("tool")
                .and_then(Value::as_str)
                .ok_or_else(|| ictl_l10n::trf!("第 {n} 步缺少 tool", "step {n} has no tool"))?;
            if !BATCH_TOOLS.contains(&tool) {
                return Err(ictl_l10n::trf!(
                    "第 {n} 步:批量里不能用 {tool},可用:{}",
                    "step {n}: {tool} can't be used in a batch; allowed: {}",
                    BATCH_TOOLS.join(ictl_l10n::tr!("、", ", "))
                ));
            }
            if tool == "wait" {
                if let Some(k) = obj.keys().find(|k| !matches!(k.as_str(), "tool" | "seconds")) {
                    return Err(ictl_l10n::trf!(
                        "第 {n} 步:wait 没有参数 {k},它的参数:tool、seconds",
                        "step {n}: wait has no argument {k}; its arguments: tool, seconds"
                    ));
                }
                let secs = Args(obj).f("seconds").map_err(|e| ictl_l10n::trf!("第 {n} 步:{e}", "step {n}: {e}"))?;
                if !(0.0..=5.0).contains(&secs) {
                    return Err(ictl_l10n::trf!(
                        "第 {n} 步:wait 的 seconds 要在 0 到 5 之间",
                        "step {n}: wait's seconds must be between 0 and 5"
                    ));
                }
                return Ok(Step::Wait(std::time::Duration::from_secs_f64(secs)));
            }
            let req = match check_args(tool, obj, &["tool"], false)
                .and_then(|()| to_request(tool, obj))
                .map_err(|e| ictl_l10n::trf!("第 {n} 步({tool}):{e}", "step {n} ({tool}): {e}"))?
            {
                Request::TapElement { text, index, scroll, .. } if reuse => {
                    Request::TapElement { text, index, max_age_ms: BATCH_REUSE_MS, scroll }
                }
                other => other,
            };
            req.check_required_text()
                .map_err(|e| ictl_l10n::trf!("第 {n} 步({tool}):{e}", "step {n} ({tool}): {e}"))?;
            Ok(Step::Request { tool: tool.to_owned(), req })
        })
        .collect::<Result<_, String>>()?;
    if !one_gesture {
        return Ok(steps);
    }
    // 全是坐标点击才能并成一个手势：别的步骤（按名字点、等待、输入）都要在点之间看界面或花时间
    let points = steps
        .iter()
        .map(|s| match s {
            Step::Request { req: Request::Tap { x, y, count }, .. } => Some((*x, *y, *count)),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(ictl_l10n::tr!(
            "one_gesture 只能用于步骤全是 iphone_tap(按坐标点)的批量",
            "one_gesture only works when every step is iphone_tap (tap by coordinates)"
        ))?;
    // 一个手势里每个点只点一下；要连点用 iphone_tap 的 count，不悄悄当成点一下
    if points.iter().any(|p| p.2 != 1) {
        return Err(ictl_l10n::tr!(
            "one_gesture 里的 iphone_tap 不能带 count;要在一个点连点,单独调用 iphone_tap 并给 count",
            "iphone_tap steps under one_gesture can't take count; to tap one point several times, call iphone_tap on its own with count"
        )
        .into());
    }
    let points = points.into_iter().map(|(x, y, _)| (x, y)).collect();
    Ok(vec![Step::Request { tool: "iphone_tap".into(), req: Request::TapSequence { points } }])
}
