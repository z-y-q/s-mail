use smail_backend::{connect, MailBackend};
fn main() {
    let cfg = smail_backend::ImapConfig {
        host: "127.0.0.1".into(),
        port: 1143,
        username: "test".into(),
        password: Some("testpass".into()),
        starttls: false,
        tls: false,
        use_oauth2: false,
    };
    let (b, folders) = connect(&cfg).expect("connect");
    println!("folders: {folders:?}");
    for f in ["Drafts", "INBOX"] {
        match b.fetch_envelopes(f) {
            Ok(rows) => {
                println!("{f}: {} 封", rows.len());
                for r in rows {
                    println!("  mid={:?} subj={:?}", r.message_id, r.subject);
                }
            }
            Err(e) => println!("{f}: ERR {e}"),
        }
    }
}
