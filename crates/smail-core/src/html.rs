//! HTML 邮件转 Markdown
//!
//! ## 职责
//!
//! - **HTML → Markdown**：将 HTML 邮件转换为可读 Markdown 文本
//! - **XSS 防护**：ammonia 白名单消毒（移除 script/事件/危险协议）
//! - **隐私保护**：默认删除全部远程图片（防跟踪像素）
//! - **结构保留**：标题/段落/列表/引用/链接/加粗斜体/代码块/表格
//!
//! ## 处理流程
//!
//! ```text
//! 原始 HTML
//!   ↓
//! ammonia 消毒（删 <script>/<img>，清理事件属性）
//!   ↓
//! scraper(html5ever) DOM 遍历
//!   ↓
//! Markdown 子集输出
//!   ↓
//! iced markdown widget 渲染
//! ```
//!
//! ## 图片策略（M4）
//!
//! - **远程图片**：ammonia 删除全部 `<img>`，单独提取 URL 列表供按需加载
//!   - 隐私默认：不自动加载（防跟踪像素、读回执）
//!   - 用户点击「显示图片」后，经 `LoadImages` 命令按需下载
//! - **内嵌图片**（cid:）：由 Reader 线程在解析 multipart 时提取，UI 自动显示
//!
//! ## 渲染端
//!
//! iced markdown widget，链接点击由 UI 层接手打开浏览器

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

/// HTML 原样渲染的消毒出口（系统网页视图加载用；与 markdown 转换共用白名单底座）
///
/// 与 `html_to_markdown` 的「删 img」隐私默认一致：`load_remote=false` 时
/// 远程图片整棵删除（防跟踪像素）；`load_remote=true`（用户点过「显示图片」）
/// 保留。其余按邮件排版需要放开、按攻击面收紧：
///
/// - 放开：`<style>` 标签与 `style`/`class` 属性（真实邮件排版全靠 CSS）、
///   表格跨行跨列（colspan/rowspan）、`data:` 图片（离线内嵌，不联网）
/// - 收紧：`<script>` 内容整体删除；`a href` 的 `data:`/`javascript:` 协议改写
///   为 `#`（网页视图侧另禁导航兜底，两层防护互不依赖）
///
/// 兜底纵深：调用方还须禁 JS + 拦导航（smail-ui webview 模块），此处只管
/// 「消毒后的 HTML 拿浏览器直接打开也安全」这一层。
pub fn sanitized_html_for_webview(html: &str, load_remote: bool) -> String {
    let mut b = ammonia::Builder::default();
    // <style> 保留（邮件排版核心）；script 仍在 clean_content_tags 里删除。
    // 默认 clean_content_tags 含 style（文本转换场景是正确默认），原样渲染
    // 场景必须移出（留在两边会触发 ammonia 的配置一致性断言）
    b.rm_clean_content_tags(["style"]);
    b.add_tags(["style"]);
    b.add_generic_attributes(["style", "class"]);
    for t in ["td", "th"] {
        b.add_tag_attributes(t, ["colspan", "rowspan"]);
    }
    // data: 仅供图片离线内嵌；链接上的 data:/javascript: 一律改写（见 attribute_filter）
    b.add_url_schemes(["data"]);
    b.attribute_filter(|element, attribute, value| {
        if element == "a" && attribute == "href" {
            let v = value.trim().to_lowercase();
            if v.starts_with("data:") || v.starts_with("javascript:") {
                return Some(std::borrow::Cow::Borrowed("#"));
            }
        }
        Some(std::borrow::Cow::Borrowed(value))
    });
    if !load_remote {
        b.rm_tags(["img"]);
    }
    b.clean(html).to_string()
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
    // 超长无空格串（跟踪链接、base64）插零宽断点：iced text 按词换行，
    // 单个超长词会撑破容器把阅读窗右缘推出视口
    break_long_tokens(normalized.trim())
}

/// 超长无空格连续串（跟踪链接、base64）插零宽断点 U+200B：
/// iced 的 text 只按词换行，单个超长词会把容器撑破、把阅读窗右缘推出
/// 视口（信头日期被裁切的根因）。含 markdown 链接语法的行跳过——
/// 插入会破坏 `[文本](URL)` 的点击结构
fn break_long_tokens(s: &str) -> String {
    if s.contains("](") {
        return s.to_string();
    }
    // 按行处理：列表/标题/段落的换行结构必须保留（否则 markdown 塌成一行）
    s.lines()
        .map(|line| {
            // 行首缩进原样保留（嵌套列表的层级缩进是 markdown 语义）
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            let mut out = String::from(indent);
            for (n, word) in line[indent_len..].split_whitespace().enumerate() {
                if n > 0 {
                    out.push(' ');
                }
                if word.chars().count() > 40 {
                    for (i, ch) in word.chars().enumerate() {
                        out.push(ch);
                        if (i + 1) % 24 == 0 && i + 1 < word.chars().count() {
                            out.push('\u{200b}');
                        }
                    }
                } else {
                    out.push_str(word);
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 渲染递归深度上限：恶意邮件可嵌套上万层标签。超过后不再下钻——
/// · 栈安全（递归帧有界）
/// · 渲染成本 O(宽度×上限) 而非 O(节点数×深度)
/// · 64 层嵌套已远超人类可读邮件结构，内容丢弃无感知
const MAX_RENDER_DEPTH: usize = 64;

fn render_children<'a, I>(children: I, out: &mut String, list_depth: usize)
where
    I: Iterator<Item = ego_tree::NodeRef<'a, Node>>,
{
    render_children_d(children, out, list_depth, 0)
}

fn render_children_d<'a, I>(children: I, out: &mut String, list_depth: usize, depth: usize)
where
    I: Iterator<Item = ego_tree::NodeRef<'a, Node>>,
{
    if depth > MAX_RENDER_DEPTH {
        return;
    }
    for node in children {
        match node.value() {
            Node::Text(t) => {
                // 断开 markdown 图片语法：文本里的 ![x](url) 字面量会被下游
                // iced markdown 渲染成图片并自动外发网络请求，绕过
                // 「远程图不自动加载」的隐私默认（追踪像素注入面）
                let collapsed = collapse_ws(t);
                // 纯装饰文本节点（营销邮件排版用的 | \ / --- 之类分隔符）没有
                // 信息量，留着只会在正文里连成乱纹
                if is_decoration_only(&collapsed) {
                    continue;
                }
                if collapsed.contains("![") {
                    out.push_str(&collapsed.replace("![", "!\\["));
                } else {
                    out.push_str(&collapsed);
                }
            }
            Node::Element(el) => {
                let name = el.name();
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = name[1..].parse::<usize>().unwrap_or(2);
                        let mut inner = String::new();
                        render_children_d(node.children(), &mut inner, list_depth, depth + 1);
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
                        render_children_d(node.children(), &mut inner, list_depth, depth + 1);
                        if !inner.trim().is_empty() {
                            block(out);
                            out.push_str(inner.trim());
                            out.push('\n');
                        }
                    }
                    "ul" | "ol" => {
                        render_list(node, name == "ol", out, list_depth, depth + 1);
                    }
                    "blockquote" => {
                        let mut inner = String::new();
                        render_children_d(node.children(), &mut inner, list_depth, depth + 1);
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
                        render_children_d(node.children(), &mut inner, list_depth, depth + 1);
                        let t = inner.trim();
                        if !t.is_empty() {
                            out.push_str("**");
                            out.push_str(t);
                            out.push_str("**");
                        }
                    }
                    "em" | "i" => {
                        let mut inner = String::new();
                        render_children_d(node.children(), &mut inner, list_depth, depth + 1);
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
                        render_children_d(node.children(), &mut inner, list_depth, depth + 1);
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
                        render_table(node, out, depth + 1);
                    }
                    // span/字体标签等：透明透传
                    _ => render_children_d(node.children(), out, list_depth, depth + 1),
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
    render_depth: usize,
) {
    if render_depth > MAX_RENDER_DEPTH {
        return;
    }
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
                _ => render_children_d(std::iter::once(child), &mut line, depth, render_depth),
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
                render_depth + 1,
            );
        }
    }
}

fn render_table(table: ego_tree::NodeRef<'_, Node>, out: &mut String, render_depth: usize) {
    let _ = render_depth;
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
                    render_children_d(cell.children(), &mut inner, 0, render_depth);
                    inner.trim().to_string()
                })
                .collect()
        })
        .collect();
    if rows.is_empty() {
        return;
    }
    // 现代营销邮件整封都套在 <table> 里做排版。iced 的 markdown 部件不支持
    // 表格语法，输出竖线表格只会把 | \| --- 全部原样漏进正文（乱纹）。
    // 这里改输出普通段落：一行一行，非空单元格用间隔号连接。
    block(out);
    for cells in &rows {
        let line = cells
            .iter()
            .filter(|c| !c.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join(" · ");
        if line.is_empty() {
            continue;
        }
        out.push_str(&line);
        out.push('\n');
    }
}

/// 纯装饰文本：全部字符都是排版分隔符（管道/反斜杠/斜杠/横线/圆点/星号/空白），
/// 不含任何文字或数字。营销模板用它们做竖线分隔，转 Markdown 后连成乱纹。
fn is_decoration_only(s: &str) -> bool {
    s.chars().all(|c| {
        matches!(
            c,
            '|' | '\\' | '/' | '-' | '–' | '—' | '·' | '•' | '*' | '_' | '~' | '丨' | '　'
        ) || c.is_whitespace()
    })
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
    fn webview_sanitize_keeps_style_and_kills_script() {
        let html =
            r#"<style>p{color:red}</style><p onclick="evil()">正文</p><script>alert(1)</script>"#;
        let out = sanitized_html_for_webview(html, false);
        assert!(
            out.contains("<style>p{color:red}</style>"),
            "style 保留：{out}"
        );
        assert!(out.contains("<p>正文</p>"), "事件属性剥除：{out}");
        assert!(!out.contains("alert"), "script 内容删除：{out}");
    }

    #[test]
    fn webview_sanitize_remote_images_default_off() {
        let html = r#"<p>图</p><img src="https://track.example/pixel" width="1">"#;
        let off = sanitized_html_for_webview(html, false);
        assert!(!off.contains("track.example"), "默认删远程图：{off}");
        let on = sanitized_html_for_webview(html, true);
        assert!(on.contains("track.example"), "允许后保留：{on}");
    }

    #[test]
    fn webview_sanitize_blocks_data_and_js_hrefs() {
        let html =
            r#"<a href="data:text/html,<h1> phishing">点</a><a href="javascript:evil()">再点</a>"#;
        let out = sanitized_html_for_webview(html, false);
        assert!(!out.contains("data:text"), "data: 链接改写：{out}");
        assert!(!out.contains("javascript:"), "js 协议改写：{out}");
        // data: 图片不受影响（离线内嵌是合法场景）
        let img = r#"<img src="data:image/png;base64,AAAA">"#;
        assert!(
            sanitized_html_for_webview(img, true).contains("data:image/png"),
            "data: 图片保留"
        );
    }

    #[test]
    fn webview_sanitize_keeps_table_span() {
        let html = r#"<table><tr><td colspan="2">跨列</td></tr></table>"#;
        let out = sanitized_html_for_webview(html, false);
        assert!(out.contains(r#"colspan="2""#), "表格跨列保留：{out}");
    }

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
        // 表格输出为普通行（iced markdown 不支持表格语法，竖线会原样漏出）
        assert!(md.contains("项目 · 金额"), "{md}");
        assert!(md.contains("云服务器 · ¥299.00"), "{md}");
        assert!(!md.contains("|"), "正文不得残留竖线表格语法：{md}");
        assert!(!md.contains("---"), "正文不得残留表头分隔行：{md}");
    }

    /// 营销邮件（Adobe 到期通知类）整封套 <table> 排版，单元格与段落间还夹
    /// "|\" 装饰文本节点。全部必须消化成干净的可读正文。
    #[test]
    fn table_layout_mail_and_pipe_decorations_cleaned() {
        let md = html_to_markdown(
            r#"<table><tr><td>|</td><td>您的 Adobe 帐户即将到期。</td><td>|</td></tr></table>
               <p>尊敬的 Adobe 客户：</p>
               <p>| <a href="https://example.com/login">立即登录</a> | --- | <a href="https://example.com/mgmt">管理帐户</a> |</p>"#,
        );
        assert!(md.contains("您的 Adobe 帐户即将到期。"), "{md}");
        assert!(md.contains("尊敬的 Adobe 客户："), "{md}");
        assert!(md.contains("[立即登录](https://example.com/login)"), "{md}");
        assert!(!md.contains("\\|"), "转义竖线残留：{md}");
        assert!(!md.contains(" ---"), "表头分隔残留：{md}");
    }

    /// 纯装饰文本节点（只有管道/横线/空白）直接丢弃；含文字数字的保留
    #[test]
    fn decoration_only_text_nodes_dropped() {
        let md = html_to_markdown(r#"<p>| \ / ---</p><p>客服电话 400-123-4567</p><p>——</p>"#);
        assert!(md.contains("客服电话 400-123-4567"), "{md}");
        assert!(!md.contains("|"), "{md}");
        assert!(!md.contains("---"), "{md}");
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

/// 对抗性样本（野邮件是对抗性输入）：XSS/追踪像素/markdown 注入/崩溃鲁棒性
#[cfg(test)]
mod adversarial_tests {
    use super::*;

    /// ammonia 加固面：iframe/form/meta 刷新/svg 载荷/base 注入/实体编码
    /// javascript/data URI/内联 style 的 url()——全部不得进输出
    #[test]
    fn hostile_html_never_reaches_output() {
        let samples = [
            r#"<iframe src="https://evil.example.com"></iframe>"#,
            r#"<form action="https://phish.example.com"><input type=submit></form>"#,
            r#"<meta http-equiv="refresh" content="0;url=https://evil.example.com">"#,
            r#"<svg onload="evil()"><circle r=1/></svg>"#,
            r#"<base href="https://evil.example.com/">"#,
            r#"<a href="&#106;avascript:evil()">实体编码 js 协议</a>"#,
            r#"<a href="jAvAsCrIpT:evil()">大小混淆</a>"#,
            r#"<a href="data:text/html;base64,PHNjcmlwdD4">data 协议</a>"#,
            r#"<p style="background:url(https://track.example.com/x.gif)">样式追踪</p>"#,
            r#"<script src="https://evil.example.com/x.js"></script>"#,
        ];
        for s in samples {
            let md = html_to_markdown(s);
            let low = md.to_lowercase();
            assert!(!low.contains("evil.example.com"), "追踪域外泄：{s} → {md}");
            assert!(!low.contains("javascript:"), "js 协议外泄：{s} → {md}");
            assert!(!low.contains("onload"), "事件属性外泄：{s} → {md}");
        }
    }

    /// 远程图提取只认 http/https：cid: 内嵌与 javascript: 伪图不进列表
    #[test]
    fn remote_img_urls_extraction_rules() {
        let html = r#"<img src="https://a.example.com/1.gif">
                      <img src="http://a.example.com/2.gif">
                      <img src="cid:part1.abc">
                      <img src="javascript:evil()">
                      <img src="data:image/png;base64,xx">
                      <img src=" HTTPS://UPPER.example.com/x.gif ">"#;
        let urls = remote_img_urls(html);
        assert_eq!(urls.len(), 2, "{urls:?}");
        assert!(urls.iter().all(|u| u.starts_with("http")), "{urls:?}");
    }

    /// markdown 注入面：文本节点里的 ![img](url) 会被下游 iced markdown
    /// 渲染成图片并自动外发网络请求——绕过「远程图不自动加载」的隐私
    /// 默认。html_to_markdown 必须断开图片语法（![ 前加反斜杠）。
    #[test]
    fn markdown_image_syntax_in_text_is_broken() {
        let md = html_to_markdown(r#"<p>正文 ![pixel](https://track.example.com/p.gif) 尾</p>"#);
        assert!(
            !md.contains("![pixel]"),
            "markdown 图片语法未断开（远程图自动加载绕过）：{md}"
        );
        // 断开后追踪 URL 仍可见于文本（供人工判断），但不构成图片语法
        assert!(md.contains("track.example.com"), "应保留可见文本：{md}");
    }

    /// 崩溃鲁棒性：恶意邮件可嵌套上万层标签。渲染递归有 MAX_RENDER_DEPTH
    /// 保护（栈有界 + 渲染成本 O(宽度×上限)）；html5ever/ammonia 解析成本
    /// ~n^1.8（第三方，已知边界——在 Reader 线程一次性付出，UI 不卡）。
    /// 用现实对抗规模钉住：不崩溃、栈不溢出。
    #[test]
    fn pathological_deep_nesting_does_not_crash() {
        let n = 3_000;
        // 浅层正文 + 万层嵌套：渲染不崩溃（栈有界），浅层内容保留，
        // 超深内容按设计丢弃（人类邮件不会嵌 64 层以上）
        let html = format!(
            "<p>浅层正文</p>{}<p>最深处</p>{}",
            "<div>".repeat(n),
            "</div>".repeat(n)
        );
        let md = html_to_markdown(&html); // 栈溢出 = 测试进程 abort（红）
        assert!(md.contains("浅层正文"), "{md}");
        assert!(
            !md.contains("最深处"),
            "超深内容应被丢弃：{}…",
            &md[..md.len().min(60)]
        );

        // 嵌套列表递归（render_list 路径）同样受保护
        let list_html = format!("{}<li>x</li>{}", "<ul>".repeat(n), "</ul>".repeat(n));
        let _ = html_to_markdown(&list_html);

        // 不平衡标签（未闭合）同样不崩溃
        let _ = html_to_markdown(&"<div>".repeat(n));
    }

    /// 深度上限不误伤正常邮件：64 层以内的嵌套内容完整保留
    #[test]
    fn normal_nesting_depth_still_rendered() {
        let mut html = String::new();
        for _ in 0..60 {
            html.push_str("<div>");
        }
        html.push_str("<p>六十层内的正文</p>");
        let md = html_to_markdown(&html);
        assert!(md.contains("六十层内的正文"), "{md}");
    }
}

#[cfg(test)]
mod long_token_tests {
    use super::break_long_tokens;

    #[test]
    fn inserts_zwsp_in_long_words() {
        let url = format!("https://x.example.com/{}", "a".repeat(120));
        let out = break_long_tokens(&url);
        assert!(out.contains('\u{200b}'), "长词必须插入零宽断点");
        assert!(!out.contains('\n'), "只插零宽断点不换行");
        let no_zw = out.replace('\u{200b}', "");
        assert_eq!(no_zw, url, "剥除断点后内容不变");
    }

    #[test]
    fn short_words_untouched() {
        let s = "普通 正文 保持 原样";
        assert_eq!(break_long_tokens(s), s);
    }

    #[test]
    fn link_lines_skipped() {
        let s = "[限时抢购](https://links.example.com/u/click?_t=aaaa)";
        assert_eq!(break_long_tokens(s), s, "链接行不插断点（保护点击结构）");
    }
}

/// 撰写器 markdown → HTML（发送侧 multipart/alternative 用）。
/// 手写轻量转换：先转义 HTML 实体防注入，再按行处理段落/列表/引用，
/// 行内支持 **粗**、*斜*、`code` 与裸链接自动成链。只覆盖撰写器用户
/// 实际会产出的语法，不做完整 markdown 规范
pub fn markdown_to_html(md: &str) -> String {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn inline(s: &str) -> String {
        let e = esc(s);
        // autolink：http(s):// 开头到空白为止
        let mut out = String::new();
        for (i, tok) in e.split(' ').enumerate() {
            if i > 0 {
                out.push(' ');
            }
            if tok.starts_with("http://") || tok.starts_with("https://") {
                out.push_str(&format!("<a href=\"{tok}\">{tok}</a>"));
            } else {
                out.push_str(tok);
            }
        }
        // 行内标记（从重到轻，避免 ** 被当 *）
        for (pat, open, close) in [
            ("**", "<strong>", "</strong>"),
            ("*", "<em>", "</em>"),
            ("`", "<code>", "</code>"),
        ] {
            let mut s2 = String::new();
            let mut parts = out.split(pat);
            if let Some(first) = parts.next() {
                s2.push_str(first);
            }
            let mut odd = false;
            for part in parts {
                if odd {
                    s2.push_str(close);
                    s2.push_str(part);
                } else {
                    s2.push_str(open);
                    s2.push_str(part);
                }
                odd = !odd;
            }
            out = s2;
        }
        out
    }

    let mut html = String::from("<html><body>\n");
    let mut in_list = false;
    let mut in_para = false;
    fn close_para(html: &mut String, in_para: bool) {
        if in_para {
            html.push_str("</p>\n");
        }
    }
    for line in md.lines() {
        let t = line.trim();
        if t.is_empty() {
            close_para(&mut html, in_para);
            in_para = false;
            continue;
        }
        // 列表行（- 或 * 开头）
        if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            close_para(&mut html, in_para);
            in_para = false;
            if !in_list {
                html.push_str("<ul>\n");
                in_list = true;
            }
            html.push_str(&format!("<li>{}</li>\n", inline(item)));
            continue;
        }
        if in_list {
            html.push_str("</ul>\n");
            in_list = false;
        }
        // 引用
        if let Some(q) = t.strip_prefix("> ") {
            html.push_str(&format!("<blockquote>{}</blockquote>\n", inline(q)));
            continue;
        }
        // 标题（# 到 ###）
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=3).contains(&hashes) {
            let text = t[hashes..].trim();
            if !text.is_empty() {
                close_para(&mut html, in_para);
                in_para = false;
                html.push_str(&format!("<h{hashes}>{}</h{hashes}>\n", inline(text)));
                continue;
            }
        }
        // 普通段落：段内换行 <br>
        if !in_para {
            html.push_str("<p>");
            in_para = true;
        } else {
            html.push_str("<br>\n");
        }
        html.push_str(&inline(t));
    }
    if in_list {
        html.push_str("</ul>\n");
    }
    close_para(&mut html, in_para);
    html.push_str("</body></html>");
    html
}

#[cfg(test)]
mod markdown_html_tests {
    use super::markdown_to_html;

    #[test]
    fn paragraph_and_bold_and_link() {
        let html = markdown_to_html("你好 **世界**\n\n访问 https://x.example.com 了解更多");
        assert!(html.contains("<p>你好 <strong>世界</strong></p>"), "{html}");
        assert!(html.contains("<a href=\"https://x.example.com\""), "{html}");
        assert!(html.starts_with("<html><body>"), "{html}");
    }

    #[test]
    fn list_and_heading_and_quote() {
        let html = markdown_to_html("- 甲\n- 乙\n\n## 标题\n\n> 引用句");
        assert!(
            html.contains("<ul>\n<li>甲</li>\n<li>乙</li>\n</ul>"),
            "{html}"
        );
        assert!(html.contains("<h2>标题</h2>"), "{html}");
        assert!(html.contains("<blockquote>引用句</blockquote>"), "{html}");
    }

    #[test]
    fn html_entities_escaped() {
        let html = markdown_to_html("<script>alert(1)</script> a & b");
        assert!(!html.contains("<script>"), "正文必须转义：{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("a &amp; b"), "{html}");
    }

    #[test]
    fn plain_text_stays_plain_paragraph() {
        let html = markdown_to_html("第一行\n第二行");
        assert!(html.contains("<p>第一行<br>\n第二行</p>"), "{html}");
    }
}
