//! 附件：落盘保存

use crate::protocol::Event;
use crate::reader::extract_attachment_bytes;
use crate::ServiceInner;
use std::path::PathBuf;

impl ServiceInner {
    pub(crate) fn save_attachment(&self, hash: i64, name: String) {
        // 内存护栏：超大邮件/附件不做全量进 RAM 的提取（用户仍可导出 .eml）
        if let Ok(meta) = std::fs::metadata(self.body_path(hash)) {
            const MAX_EML: u64 = 512 * 1024 * 1024;
            if meta.len() > MAX_EML {
                self.emit(Event::Status(self.trf(
                    "邮件过大（{} MB）不做附件提取，请用「存为 .eml」",
                    "Mail too large ({} MB) for attachment extraction; use Save .eml",
                    &[&(meta.len() / 1024 / 1024)],
                )));
                return;
            }
        }
        let bytes = match std::fs::read(self.body_path(hash)) {
            Ok(b) => b,
            Err(e) => {
                self.emit(Event::Status(self.trf(
                    "读邮件缓存失败：{}",
                    "Failed to read mail cache: {}",
                    &[&e],
                )));
                return;
            }
        };
        let Some(data) = extract_attachment_bytes(&bytes, &name) else {
            self.emit(Event::Status(self.trf(
                "找不到附件 {name:?}",
                "Attachment {} not found",
                &[],
            )));
            return;
        };
        let dir: PathBuf = std::env::var_os("SMAIL_DOWNLOADS")
            .map(Into::into)
            .unwrap_or_else(|| {
                let mut d = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
                d.push("Downloads");
                d
            });
        let _ = std::fs::create_dir_all(&dir);
        let safe: String = name
            .chars()
            .map(|c| if c == '/' || c == '\\' { '_' } else { c })
            .collect();
        let mut target = dir.join(&safe);
        let mut n = 1;
        while target.exists() {
            let stem = safe.trim_end_matches(|c| c != '.').trim_end_matches('.');
            target = dir.join(format!("{stem}-{n}"));
            n += 1;
        }
        if let Err(e) = std::fs::write(&target, data) {
            self.emit(Event::Status(self.trf(
                "附件写入失败：{}",
                "Failed to write attachment: {}",
                &[&e],
            )));
            return;
        }
        let path = target.display().to_string();
        self.emit(Event::AttachmentSaved { path: path.clone() });
        #[cfg(target_os = "macos")]
        if std::env::var_os("SMAIL_NO_REVEAL").is_none() {
            let _ = std::process::Command::new("open")
                .arg("-R")
                .arg(&target)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}
