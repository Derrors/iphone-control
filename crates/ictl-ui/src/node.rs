//! [`Source`] 和 XML 之间：读 WDA 的 `/source`，写出来存样本（`debug … --out`）

use std::fmt::Write;

use super::*;
use quick_xml::{XmlVersion, events::Event};

/// 再深的节点不读（标成没读完）：真机的树不到 100 层，层数没有上限的话递归会用完栈
const MAX_DEPTH: usize = 512;

impl Node {
    /// 自己和所有后代，先序（XML 里的顺序）
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        preorder(vec![self])
    }

    /// 标签、名字、值里有等于 `s` 的
    pub(super) fn says(&self, s: &str) -> bool {
        self.label == s || self.name == s || self.value == s
    }
}

fn preorder(mut stack: Vec<&Node>) -> impl Iterator<Item = &Node> {
    std::iter::from_fn(move || {
        let n = stack.pop()?;
        stack.extend(n.children.iter().rev());
        Some(n)
    })
}

impl Source {
    /// 所有节点，先序
    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        preorder(self.roots.iter().rev().collect())
    }

    /// WDA `/source` 的 XML（`XCUIElementType…` 标签，带 `type`、`label`、`name`、`value`、`x`、`y`、`width`、
    /// `height`）。类型取 `type`，没有就取标签名。断了或出错时留下已读到的部分，标成没读完
    pub fn from_xml(xml: &str) -> Self {
        let mut reader = quick_xml::Reader::from_str(xml);
        let mut src = Source::default();
        // 还没闭合的节点
        let mut open: Vec<Node> = Vec::new();
        // 太深而跳过的、还没闭合的层数
        let mut skipped = 0usize;
        let attach = |src: &mut Source, open: &mut Vec<Node>, n: Node| match open.last_mut() {
            Some(p) => p.children.push(n),
            None => src.roots.push(n),
        };
        loop {
            let (e, empty) = match reader.read_event() {
                Ok(Event::Start(e)) => (e, false),
                Ok(Event::Empty(e)) => (e, true),
                Ok(Event::End(_)) => {
                    if skipped > 0 {
                        skipped -= 1;
                    } else if let Some(n) = open.pop() {
                        attach(&mut src, &mut open, n);
                    }
                    continue;
                }
                Ok(Event::Eof) => {
                    src.incomplete |= !open.is_empty();
                    break;
                }
                Err(e) => {
                    // 解析到一半出错：已解析的照常返回，但要留痕，否则「没找到元素」看起来像真结果
                    tracing::warn!(
                        "{}",
                        ictl_l10n::log_trf!(
                            "元素树 XML 解析出错（位置 {}），只用了前面的部分：{e}",
                            "Error parsing the element tree XML (at {}), only the part before it is used: {e}",
                            reader.buffer_position()
                        )
                    );
                    src.incomplete = true;
                    break;
                }
                Ok(_) => continue,
            };
            if skipped > 0 || open.len() >= MAX_DEPTH {
                src.incomplete = true;
                skipped += usize::from(!empty);
                continue;
            }
            let mut attr: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            for a in e.attributes().flatten() {
                let k = a.key.as_ref().to_owned();
                let v = a.normalized_value(XmlVersion::Implicit1_0).map(|c| c.into_owned()).unwrap_or_default();
                attr.insert(k, v);
            }
            let text = |k: &str| attr.get(k).map(|s| s.trim().to_owned()).unwrap_or_default();
            let num = |k: &str| attr.get(k).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            let kind = attr.get("type").cloned().unwrap_or_else(|| e.name().as_ref().to_owned());
            let n = Node {
                kind: kind.strip_prefix("XCUIElementType").unwrap_or(&kind).to_owned(),
                label: text("label"),
                name: text("name"),
                value: text("value"),
                frame: Rect { x: num("x"), y: num("y"), w: num("width"), h: num("height") },
                children: Vec::new(),
            };
            if empty {
                attach(&mut src, &mut open, n);
            } else {
                open.push(n);
            }
        }
        // what was left open, with what it holds
        while let Some(n) = open.pop() {
            attach(&mut src, &mut open, n);
        }
        src
    }

    /// 写成 WDA `/source` 那样的 XML（[`Self::from_xml`] 读回来是同样的树）
    pub fn to_xml(&self) -> String {
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
        for r in &self.roots {
            write_node(&mut out, r, 0);
        }
        out
    }
}

fn write_node(out: &mut String, n: &Node, depth: usize) {
    let kind = &n.kind;
    let _ = write!(out, "<XCUIElementType{kind} type=\"XCUIElementType{kind}\"");
    for (k, v) in [("label", &n.label), ("name", &n.name), ("value", &n.value)] {
        if !v.is_empty() {
            let _ = write!(out, " {k}=\"");
            esc(v, out);
            out.push('"');
        }
    }
    let f = n.frame;
    let _ = write!(out, " x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"", f.x, f.y, f.w, f.h);
    // no deeper than `from_xml` reads
    if n.children.is_empty() || depth + 1 >= MAX_DEPTH {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for c in &n.children {
        write_node(out, c, depth + 1);
    }
    let _ = write!(out, "</XCUIElementType{kind}>");
}

/// 属性值：XML 1.0 里能写的控制字符只有换行、回车和 tab，写成字符引用（照原样写会被读成空格）；别的去掉
fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' | '\r' | '\t' => {
                let _ = write!(out, "&#{};", c as u32);
            }
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
}
