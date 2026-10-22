//! HTML 邮件 → markdown 结构块（M4 管线，DESIGN §5）。
//!
//! 流程：ammonia 白名单消毒（去 script/事件/危险协议，**删全部 `<img>`**——
//! 远程图默认禁，防跟踪像素）→ scraper(html5ever) DOM 遍历 → markdown 子集：
//! 标题/段落/列表/引用/链接/加粗斜体/代码块/表格(尽力而为)。
//!
//! 渲染端：iced markdown widget（链接点击由 UI 层接手开浏览器）。

use scraper::{Html, Node};

/// HTML 里远程图片 URL（http/https；消毒前提取——ammonia 会删 img，
/// 单独抽出来供按需加载）
pub fn remote_img_urls(html: &str) -> Vec<String> {
    let frag = Html::parse_fragment(html);
    frag.root_element()
        .descendants()
        .filter_map(|n| match n.value() {
            Node::Element(el) if el.name() == "img" => el.attr("src"),
            _ => None,
        })
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .collect()
}

/// 主入口：HTML → markdown 文本（供 markdown widget 渲染）
pub fn html_to_markdown(html: &str) -> String {
    // 消毒：默认白名单基础上删掉 img（M4 图片策略：一律不加载）
    let sanitized = ammonia::Builder::default()
        .rm_tags(["img"])
        .clean(html)
        .to_string();

    let frag = Html::parse_fragment(&sanitized);
    let mut out = String::with_capacity(sanitized.len());
    render_children(frag.root_element().children(), &mut out, 0);
    // 归一：多余空行压缩成一段、收尾空白清理
    let mut normalized = String::with_capacity(out.len());
    let mut blanks = 0;
    for line in out.lines() {
        let l = line.trim_end();
        if l.is_empty() {
            blanks += 1;
            if blanks <= 1 {
                normalized.push('\n');
            }
        } else {
            blanks = 0;
            normalized.push_str(l);
            normalized.push('\n');
        }
    }
    normalized.trim().to_string()
}

fn render_children<'a, I>(children: I, out: &mut String, list_depth: usize)
where
    I: Iterator<Item = ego_tree::NodeRef<'a, Node>>,
{
    for node in children {
        match node.value() {
            Node::Text(t) => {
                out.push_str(&collapse_ws(t));
            }
            Node::Element(el) => {
                let name = el.name();
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = name[1..].parse::<usize>().unwrap_or(2);
                        let mut inner = String::new();
                        render_children(node.children(), &mut inner, list_depth);
                        block(out);
                        out.push_str(&"#".repeat(level.min(6)));
                        out.push(' ');
                        out.push_str(inner.trim());
                        out.push('\n');
                    }
                    "p" | "div" | "section" | "article" | "td" | "th"
                        if name != "td" && name != "th" =>
                    {
                        // 块级容器：整体作为段落
                        let mut inner = String::new();
                        render_children(node.children(), &mut inner, list_depth);
                        if !inner.trim().is_empty() {
                            block(out);
                            out.push_str(inner.trim());
                            out.push('\n');
                        }
                    }
                    "ul" | "ol" => {
                        render_list(node, name == "ol", out, list_depth);
                    }
                    "blockquote" => {
                        let mut inner = String::new();
                        render_children(node.children(), &mut inner, list_depth);
                        block(out);
                        for line in inner.trim().lines() {
                            out.push_str("> ");
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                    "pre" => {
                        let code: String = node
                            .children()
                            .filter_map(|c| match c.value() {
                                Node::Text(t) => Some(t.text.to_string()),
                                _ => None,
                            })
                            .collect();
                        block(out);
                        out.push_str("```\n");
                        out.push_str(code.trim());
                        out.push_str("\n```\n");
                    }
                    "br" => out.push_str("  \n"),
                    "hr" => {
                        block(out);
                        out.push_str("---\n");
                    }
                    "strong" | "b" => {
                        let mut inner = String::new();
                        render_children(node.children(), &mut inner, list_depth);
                        let t = inner.trim();
                        if !t.is_empty() {
                            out.push_str("**");
                            out.push_str(t);
                            out.push_str("**");
                        }
                    }
                    "em" | "i" => {
                        let mut inner = String::new();
                        render_children(node.children(), &mut inner, list_depth);
                        let t = inner.trim();
                        if !t.is_empty() {
                            out.push('*');
                            out.push_str(t);
                            out.push('*');
                        }
                    }
                    "a" => {
                        let href = el.attr("href").unwrap_or("").trim();
                        let mut inner = String::new();
                        render_children(node.children(), &mut inner, list_depth);
                        let label = if inner.trim().is_empty() {
                            href.to_string()
                        } else {
                            inner.trim().to_string()
                        };
                        if href.is_empty() {
                            out.push_str(&label);
                        } else {
                            out.push_str(&format!("[{label}]({href})"));
                        }
                    }
                    "table" => {
                        render_table(node, out, list_depth);
                    }
                    // span/字体标签等：透明透传
                    _ => render_children(node.children(), out, list_depth),
                }
            }
            _ => {}
        }
    }
}

fn render_list<'a>(
    list: ego_tree::NodeRef<'a, Node>,
    ordered: bool,
    out: &mut String,
    depth: usize,
) {
    let indent = "  ".repeat(depth);
    let mut idx = 1usize;
    let mut wrote = false;
    for li in list
        .children()
        .filter(|c| matches!(c.value(), Node::Element(el) if el.name() == "li"))
    {
        if !wrote {
            block(out);
            wrote = true;
        }
        let marker = if ordered {
            let m = format!("{idx}. ");
            idx += 1;
            m
        } else {
            "- ".to_string()
        };
        // li 内部可能嵌套列表：先收集非列表内容为行，列表递归
        let mut line = String::new();
        let mut sublists: Vec<ego_tree::NodeRef<'a, Node>> = Vec::new();
        for child in li.children() {
            match child.value() {
                Node::Element(el) if el.name() == "ul" || el.name() == "ol" => sublists.push(child),
                _ => render_children(std::iter::once(child), &mut line, depth),
            }
        }
        out.push_str(&indent);
        out.push_str(&marker);
        out.push_str(line.trim());
        out.push('\n');
        for sub in sublists {
            render_list(
                sub,
                matches!(sub.value(), Node::Element(e) if e.name() == "ol"),
                out,
                depth + 1,
            );
        }
    }
}

fn render_table(table: ego_tree::NodeRef<'_, Node>, out: &mut String, depth: usize) {
    let _ = depth;
    // html5ever 会自动插入 tbody/thead/tfoot：展开一层再取 tr
    let trs: Vec<_> = table
        .children()
        .flat_map(|c| match c.value() {
            Node::Element(el)
                if el.name() == "tbody" || el.name() == "thead" || el.name() == "tfoot" =>
            {
                c.children().collect::<Vec<_>>()
            }
            _ => vec![c],
        })
        .filter(|c| matches!(c.value(), Node::Element(el) if el.name() == "tr"))
        .collect();
    let rows: Vec<Vec<String>> = trs
        .into_iter()
        .filter(|tr| {
            tr.children().any(
                |c| matches!(c.value(), Node::Element(e) if e.name() == "td" || e.name() == "th"),
            )
        })
        .map(|tr| {
            tr.children()
                .filter(|c| {
                    matches!(c.value(), Node::Element(el) if el.name() == "td" || el.name() == "th")
                })
                .map(|cell| {
                    let mut inner = String::new();
                    render_children(cell.children(), &mut inner, 0);
                    inner.trim().replace('|', "\\|")
                })
                .collect()
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    block(out);
    let mut header_done = false;
    for (i, cells) in rows.iter().enumerate() {
        if cells.is_empty() {
            continue;
        }
        out.push_str("| ");
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
        // 第一行后补分隔行（markdown 表头）
        if i == 0 {
            out.push('|');
            for _ in cells {
                out.push_str(" --- |");
            }
            out.push('\n');
            header_done = true;
        }
    }
    let _ = header_done;
}

/// 确保块级元素之间有空行（markdown 段落语义）
fn block(out: &mut String) {
    if out.is_empty() {
        return;
    }
    if !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else {
            out.push_str("\n\n");
        }
    }
}

/// HTML 空白折叠（连续空白 → 单空格；&nbsp; html5ever 已解码为 U+00A0）
fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for c in s.chars() {
        if c.is_whitespace() || c == '\u{a0}' {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            } else if in_ws && !out.is_empty() {
                // 已以空格结尾，跳过
            }
            in_ws = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_paragraph_link() {
        let md = html_to_markdown(
            r#"<h2>发票通知</h2><p>您的 <strong>五月账单</strong> 已出，请<a href="https://example.com/pay">查看详情</a>。</p>"#,
        );
        assert!(md.contains("## 发票通知"), "标题：{md}");
        assert!(md.contains("**五月账单**"), "加粗：{md}");
        assert!(
            md.contains("[查看详情](https://example.com/pay)"),
            "链接：{md}"
        );
    }

    #[test]
    fn lists_nested() {
        let md = html_to_markdown(
            r#"<ul><li>第一项</li><li>第二项<ol><li>嵌套甲</li><li>嵌套乙</li></ol></li></ul>"#,
        );
        assert!(md.contains("- 第一项"), "{md}");
        assert!(md.contains("- 第二项"), "{md}");
        assert!(md.contains("  1. 嵌套甲"), "嵌套有序列表：{md}");
        assert!(md.contains("  2. 嵌套乙"), "{md}");
    }

    #[test]
    fn blockquote_and_table() {
        let md = html_to_markdown(
            r#"<blockquote>原邮件内容引用</blockquote>
               <table><tr><th>项目</th><th>金额</th></tr>
               <tr><td>云服务器</td><td>¥299.00</td></tr></table>"#,
        );
        assert!(md.contains("> 原邮件内容引用"), "{md}");
        assert!(md.contains("| 项目 | 金额 |"), "{md}");
        assert!(md.contains("| --- | --- |"), "{md}");
        assert!(md.contains("| 云服务器 | ¥299.00 |"), "{md}");
    }

    #[test]
    fn script_and_images_removed() {
        let md = html_to_markdown(
            r#"<p>正文</p><script>alert('xss')</script>
               <img src="https://tracker.example.com/pixel.gif">
               <p onclick="evil()">第二段</p><a href="javascript:evil()">坏链</a>"#,
        );
        assert!(!md.contains("alert"), "script 删除：{md}");
        assert!(!md.contains("img"), "图片删除：{md}");
        assert!(!md.contains("onclick"), "事件删除：{md}");
        assert!(!md.contains("javascript:"), "危险协议删除：{md}");
        assert!(md.contains("正文") && md.contains("第二段"), "{md}");
    }

    #[test]
    fn chinese_commercial_mail_realistic() {
        let md = html_to_markdown(
            r#"<html><body>
               <h3>订单发货通知</h3>
               <p>您好，您的订单 <b>SO-20261018-001</b> 已发货。</p>
               <ul><li>承运：顺丰速运</li><li>单号：SF1234567890</li></ul>
               <p><a href="https://example.com/track?no=SF1234567890">物流跟踪</a></p>
               </body></html>"#,
        );
        assert!(md.contains("### 订单发货通知"), "{md}");
        assert!(md.contains("**SO-20261018-001**"), "{md}");
        assert!(md.contains("- 承运：顺丰速运"), "{md}");
        assert!(md.contains("[物流跟踪]("), "{md}");
    }
}
