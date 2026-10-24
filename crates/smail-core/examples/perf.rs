//! perf：核心性能基线（CI 门禁用；无网络无 GUI，纯本地确定性测量）
//!
//! ## 指标（1 万封规模，代理用户可感知路径）
//!
//! - `seed_ms`：建库 + 信封批量写入 + 信封级 FTS（首同步成本代理）
//! - `open_ms`：冷开已有库（应用启动路径代理；median×5）
//! - `search_ms`：FTS5 全文检索 + snippet（搜索框路径；median×5）
//! - `list_ms`：文件夹列表查询 500 封（UI 列表数据源；median×5）
//!
//! ## 运行
//!
//! ```sh
//! cargo run -p smail-core --release --example perf
//! ```
//!
//! 超基线退出码 1。阈值 = 本机实测 × ~3 冗余（防 CI 机器抖动误报），
//! 只拦量级级回归（10× 变慢、索引膨胀），不追求精确对比。
//! GUI 冷启动（~0.7s）与稳态 RSS（~100MB）无法在无头 CI 测量，
//! 保持 DOGFOOD.md 手工流程。

use std::time::{Duration, Instant};

use smail_store::{MessageRow, Store};

const N: usize = 10_000;
/// 检索测量给多少封建正文索引（后台索引器的稳态代理）
const BODY_INDEXED: usize = 2_000;

/// 阈值：本机（Apple Silicon，release）实测 × ~3。
/// 只拦截量级回归（计划走错索引、N+1 查询、FTS 膨胀类），不追精度。
const THRESHOLDS: [(&str, u64); 4] = [
    ("seed_ms", 8_000),
    ("open_ms", 500),
    ("search_ms", 400),
    ("list_ms", 300),
];

fn row(i: usize) -> MessageRow {
    let sender = i % 97;
    MessageRow {
        envelope_hash: i as i64,
        folder: if i.is_multiple_of(10) {
            "Sent".into()
        } else {
            "INBOX".into()
        },
        message_id: format!("<m{i}@perf>"),
        subject: format!("第{i}号发票与报销通知"),
        from_addr: format!("finance{sender}@corp.example.com"),
        date_unix: 1_700_000_000 + i as i64,
        seen: i.is_multiple_of(3),
        flagged: i.is_multiple_of(50),
        has_body: false,
        in_reply_to: String::new(),
    }
}

fn body(i: usize) -> String {
    // 合成正文：混入检索目标词 + 噪声词，模拟真实中文商业邮件
    format!(
        "您好，第{i}季度的发票已开具，请核对报销金额后回复。附件为明细清单，\
         如有疑问请联系财务部。本邮件由系统自动发送，请勿直接回复。{}",
        "琐碎噪声内容重复填充。".repeat(i % 17 + 3)
    )
}

fn median(v: &mut [Duration]) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn main() {
    let dir = std::env::temp_dir().join(format!("smail-perf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("perf.db");

    // ---- seed：建库 + 批量信封 + 信封 FTS + 部分正文 FTS ----
    let t = Instant::now();
    {
        let store = Store::open(&db).unwrap();
        let rows: Vec<MessageRow> = (0..N).map(row).collect();
        store.upsert_new_batch(&rows).unwrap();
        for r in rows.iter().take(BODY_INDEXED) {
            store
                .index_envelope(r.envelope_hash, &r.subject, &r.from_addr)
                .unwrap();
            store
                .index_body(r.envelope_hash, &body(r.envelope_hash as usize))
                .unwrap();
        }
        for r in rows.iter().skip(BODY_INDEXED) {
            store
                .index_envelope(r.envelope_hash, &r.subject, &r.from_addr)
                .unwrap();
        }
    } // drop 走 optimize pragma
    let seed = t.elapsed();

    // ---- open：冷开已有库（median×5）----
    let mut opens = [Duration::ZERO; 5];
    for slot in &mut opens {
        let t = Instant::now();
        let store = Store::open(&db).unwrap();
        *slot = t.elapsed();
        drop(store);
    }
    let open = median(&mut opens);

    // ---- search / list（median×5）----
    let store = Store::open(&db).unwrap();
    let mut searches = [Duration::ZERO; 5];
    for slot in &mut searches {
        let t = Instant::now();
        let hits = store.search_with_snippets("发票 报销", 50).unwrap();
        *slot = t.elapsed();
        assert!(!hits.is_empty(), "检索必须有命中（否则测了个寂寞）");
    }
    let search = median(&mut searches);

    let mut lists = [Duration::ZERO; 5];
    for slot in &mut lists {
        let t = Instant::now();
        let rows = store.list_folder("INBOX", 500).unwrap();
        *slot = t.elapsed();
        assert_eq!(rows.len(), 500, "列表应满页");
    }
    let list = median(&mut lists);
    drop(store);
    // WAL 在最后连接关闭时 checkpoint：体积须在 drop 后读（否则主库文件还是空的）
    let db_mb = std::fs::metadata(&db)
        .map(|m| m.len() / 1024 / 1024)
        .unwrap_or(0);

    let _ = std::fs::remove_dir_all(&dir);
    let metrics = [
        ("seed_ms", seed.as_millis() as u64),
        ("open_ms", open.as_millis() as u64),
        ("search_ms", search.as_millis() as u64),
        ("list_ms", list.as_millis() as u64),
    ];
    println!("[perf] {N} 封（其中 {BODY_INDEXED} 封有正文索引），库体积 {db_mb}MB");
    let mut breached = false;
    for (name, val) in metrics {
        let limit = THRESHOLDS.iter().find(|(n, _)| *n == name).unwrap().1;
        let mark = if val > limit { "❌ 超基线" } else { "✅" };
        println!("[perf] {name} = {val} ms（基线 {limit} ms）{mark}");
        if val > limit {
            breached = true;
        }
    }
    if breached {
        eprintln!("[perf FAIL] 存在量级回归，见上");
        std::process::exit(1);
    }
    println!("[perf PASS] 核心性能在基线内");
}
