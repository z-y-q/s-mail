//! S3 · melib 0.8.13 实连 spike（docs/analysis/round-3 §2）
//!
//! 准备：
//!   cp spikes/spike-melib/config.example.toml spikes/spike-melib/config.toml   # 填真实账号
//!   cargo run -p spike-melib --release
//!
//! 验收清单（人工逐项）：
//!   [ ] 列出服务器文件夹
//!   [ ] fetch 到 config.limit 封邮件元数据（主题/发件人/日期是中文且正确）
//!   [ ] 第一封完整邮件字节数合理（非 0）
//!   [ ] watch/IDLE 期间用手机发一封信，出现 [watch] 事件
//!   [ ] 163/126：无 "Unsafe Login"（melib 是否发 IMAP ID 命令的实证）
//!
//! 备注：melib 的 IMAP 配置放在 AccountSettings.extra（server_hostname 等），
//! 这里用 TOML 字符串 + serde 反序列化构造，让所有带默认值的字段走 melib 自己的默认。

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use melib::backends::{BackendEvent, BackendEventConsumer, IsSubscribedFn, MailBackend};
use melib::conf::AccountSettings;
use melib::imap::ImapType;

#[derive(serde::Deserialize)]
struct Config {
    account: Account,
    run: Run,
}

#[derive(serde::Deserialize)]
struct Account {
    host: String,
    port: u16,
    username: String,
    password: String,
    #[serde(default)]
    starttls: bool,
    /// false = 纯文本（仅本地 dovecot 测试用，见 imap-test/）
    #[serde(default = "default_true")]
    tls: bool,
}

fn default_true() -> bool {
    true
}

#[derive(serde::Deserialize)]
struct Run {
    folder: String,
    limit: usize,
    idle_seconds: u64,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if let Err(err) = run() {
        eprintln!("\n[S3 FAILED] {err:?}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cfg_path = std::env::current_dir()?.join("config.toml");
    if !cfg_path.exists() {
        return Err(format!(
            "找不到 {}：先 `cp config.example.toml config.toml` 并填入账号（参考文件内 provider 注意事项）",
            cfg_path.display()
        )
        .into());
    }
    let cfg: Config = toml::from_str(&std::fs::read_to_string(&cfg_path)?)?;

    // AccountSettings 经 serde 构造。注意两点（melib 无文档的坑，源码实证）：
    // 1. extra 字段是 #[serde(flatten)] 的 IndexMap<String,String>：server_* 等键必须放在
    //    顶层（不能写成 [extra] 子表，否则整个表被当成一个 map 值报错）
    // 2. extra 的值必须是字符串（server_port 也要引号），melib 的 get_conf_val! 自己解析
    let settings_toml = format!(
        r#"
name = "s-mail-s3"
root_mailbox = "{folder}"
format = "imap"
identity = "{username}"
server_hostname = "{host}"
server_port = "{port}"
server_username = "{username}"
server_password = "{password}"
use_tls = "{tls}"
use_starttls = "{starttls}"
timeout = "30"
"#,
        folder = cfg.run.folder,
        username = cfg.account.username,
        host = cfg.account.host,
        port = cfg.account.port,
        password = cfg.account.password,
        starttls = cfg.account.starttls,
        tls = cfg.account.tls,
    );
    let settings: AccountSettings = toml::from_str(&settings_toml)?;

    let event_consumer = BackendEventConsumer::new(Arc::new(|_acct, ev: BackendEvent| {
        // watch 之外的后端事件（如连接池异常）打到 stderr
        eprintln!("[backend-event] {ev:?}");
    }));

    let mut backend: Box<dyn MailBackend> =
        ImapType::new(&settings, IsSubscribedFn::default(), event_consumer)?;

    // melib 独立使用的关键坑（源码实证）：imap_mailboxes() 等操作不会自动连接，
    // 必须先调 is_online()（内部完成 TCP+TLS+LOGIN+CAPABILITY），否则报 "Offline"
    println!("连接与登录…");
    futures::executor::block_on(backend.is_online()?)?;

    // 1) 文件夹列表（注意：melib 的 ResultFuture 是 Result 包 Future，需先解外层再 block_on）
    let mailboxes = futures::executor::block_on(backend.mailboxes()?)?;
    println!("== 文件夹（{} 个）==", mailboxes.len());
    let mut target = None;
    for (hash, mb) in &mailboxes {
        println!("  path={:<24} name={}", mb.path(), mb.name());
        if mb.path().eq_ignore_ascii_case(&cfg.run.folder) {
            target = Some(*hash);
        }
    }
    let target = target.ok_or_else(|| format!("文件夹 {:?} 未在服务器上找到", cfg.run.folder))?;

    // 2) 信件列表（ENVELOPE 级）
    println!("\n== fetch {:?} ==", cfg.run.folder);
    let mut stream = backend.fetch(target)?;
    let mut n_total = 0usize;
    let mut n_printed = 0usize;
    let mut first_hash = None;
    while let Some(batch) = futures::executor::block_on(stream.next()) {
        for env in batch? {
            if first_hash.is_none() {
                first_hash = Some(env.hash());
            }
            if n_printed < cfg.run.limit {
                let from = env
                    .from()
                    .first()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "<无发件人>".into());
                let seen = if env.is_seen() { "已读" } else { "未读" };
                println!(
                    "[{n_printed:4}] {:<12} {seen} {from} | {}",
                    env.date(),
                    env.subject()
                );
                n_printed += 1;
            }
            n_total += 1;
        }
    }
    println!("共 {n_total} 封（打印前 {} 封）", cfg.run.limit);

    // 3) 完整邮件一封
    if let Some(h) = first_hash {
        let bytes = futures::executor::block_on(backend.envelope_bytes_by_hash(h)?)?;
        println!("\n== 完整邮件：{h:?}，{} 字节 ==", bytes.len());
    }

    // 4) watch / IDLE
    if cfg.run.idle_seconds > 0 {
        println!(
            "\n== watch/IDLE {} 秒：现在用手机给这个账号发一封邮件，观察下方事件 ==",
            cfg.run.idle_seconds
        );
        let mut events = backend.watch()?;
        let watcher = std::thread::spawn(move || {
            while let Some(ev) = futures::executor::block_on(events.next()) {
                match ev {
                    Ok(ev) => println!("[watch] {ev:?}"),
                    Err(err) => {
                        eprintln!("[watch 错误] {err:?}");
                        break;
                    }
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(cfg.run.idle_seconds);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
        }
        std::mem::forget(watcher); // 进程退出时直接结束监视线程
    }

    println!("\n[S3 OK] 若以上清单全部打勾，melib 路线对当前 provider 验证通过");
    Ok(())
}
