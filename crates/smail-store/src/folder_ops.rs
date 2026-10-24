//! 文件夹级操作
//!
//! ## 职责
//!
//! - **删除文件夹**：delete_folder_rows（清理 messages + FTS）
//! - **重命名文件夹**：rename_folder_rows（更新本地索引路径）

use super::Store;

impl Store {
    /// 删除文件夹的全部本地记录（messages + FTS；服务器删除成功后调用）
    pub fn delete_folder_rows(&self, folder: &str) -> rusqlite::Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM messages_fts WHERE rowid IN \
             (SELECT envelope_hash FROM messages WHERE folder = ?1)",
            [folder],
        )?;
        self.conn
            .execute("DELETE FROM messages WHERE folder = ?1", [folder])?;
        Ok(n)
    }

    /// 文件夹改名：本地记录随服务器新路径更新（FTS 无 folder 概念，不动）
    pub fn rename_folder_rows(&self, old: &str, new: &str) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE messages SET folder = ?2 WHERE folder = ?1",
            (old, new),
        )
    }
}
