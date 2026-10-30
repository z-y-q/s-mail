//! 通道协议定义
//!
//! ## 职责
//!
//! 定义 UI ↔ 服务线程之间的消息协议：
//! - `Cmd`：UI 发给服务的命令（查询、操作、配置）
//! - `Event`：服务发给 UI 的事件（状态更新、数据响应、错误）
//!
//! ## 架构
//!
//! ```text
//! UI 线程 ──Cmd──> 服务线程（每账号一个）
//!         <──Event── (命令响应 + IDLE 推送)
//! ```
//!
//! ## 关键类型
//!
//! - `Cmd`：同步命令（SelectFolder、OpenMessage、SendMail 等）
//! - `Event`：异步事件（Folders、LocalList、BodyLoaded、Status、Error 等）
//! - `FolderInfo`：文件夹元数据（路径、未读数、标记状态）

use smail_store::{ContactRow, DraftRow, MessageRow, OutboxRow};

#[derive(Debug)]
pub enum Cmd {
    SelectFolder(String),
    /// UI 语言（服务线程状态/错误文案用；不改任何数据行为）
    SetLang(bool),
    OpenMessage(i64),
    Shutdown,
    SendMail {
        to: String,
        cc: String,
        bcc: String,
        subject: String,
        body: String,
        /// 附件文件路径（服务线程读文件）
        attachments: Vec<String>,
        /// M5-d：PGP 签名
        sign: bool,
        /// M5-d：PGP 加密（隐含签名）
        encrypt: bool,
    },
    SaveDraft {
        id: Option<i64>,
        to: String,
        cc: String,
        bcc: String,
        subject: String,
        body: String,
    },
    ListDrafts,
    /// 发件箱视图：全量列表（含 failed）
    ListOutbox,
    /// 发件箱：重发一条（回 pending，立即尝试）
    OutboxRetry(i64),
    /// 发件箱：删除一条
    OutboxRemove(i64),
    /// 清空废纸篓（服务器永久删除该文件夹全部邮件）
    EmptyTrash(String),
    DeleteDraft(i64),
    MarkSeen(i64, bool),
    ToggleFlagged(i64, bool),
    DeleteMessage(i64),
    // M3
    Search(String),
    /// 手动刷新：绕过 TTL 立即全量同步指定文件夹（顶栏 ↻ / F5；TTL 的安全出口）
    ForceRefresh(String),
    /// 触底加载下一页（本地索引扩大重发；服务端新邮件由 IDLE 推送）
    LoadMore(String),
    /// 一键归档：解析归档目标（无则新建 Archive）后批量移动
    Archive(Vec<i64>),
    /// 标为垃圾：解析垃圾目标（无则新建 Junk）后批量移动（v1 无自学习）
    Spam(Vec<i64>),
    /// 通讯录：全量列表（管理界面用；带名字与 ID）
    ListContactsFull,
    /// 通讯录删除条目
    DeleteContact(i64),
    /// 通讯录添加/更新（将发件人加入联系人；名字空则只存地址）
    AddContact {
        email: String,
        name: String,
    },
    /// 导出原始 .eml 到指定路径（UI 先弹保存对话框拿路径）
    ExportEml {
        hash: i64,
        path: String,
    },
    /// 按需加载远程图片（用户点击「显示图片」后；隐私默认禁远程图）
    LoadImages {
        hash: i64,
        urls: Vec<String>,
    },
    /// 只读本地索引发列表（统一收件箱聚合用；不触发服务器同步，
    /// 新邮件由 IDLE 推送）
    LocalList(String),
    SaveAttachment {
        hash: i64,
        name: String,
    },
    // ---- M7：手动移动/批量操作/文件夹管理/联系人 ----
    /// 单封移动（读信视图「移动到」）
    MoveMessage {
        hash: i64,
        dest: String,
    },
    /// 批量标记（seen/flagged 至少其一；可跨文件夹，服务按 folder 分组）
    BatchFlags {
        hashes: Vec<i64>,
        seen: Option<bool>,
        flagged: Option<bool>,
    },
    BatchMove {
        hashes: Vec<i64>,
        dest: String,
    },
    BatchDelete {
        hashes: Vec<i64>,
    },
    CreateFolder(String),
    DeleteFolder(String),
    RenameFolder {
        old_path: String,
        new_path: String,
    },
    /// 最近联系人（compose 自动补全用）
    ListContacts,
}

#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub path: String,
    pub total: u32,
    pub unseen: u32,
    /// 特殊文件夹类型（UI 图标/Drafts 重定向/Junk 动作）
    pub kind: smail_backend::FolderKind,
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // BodyLoaded 带图片字节——事件流一次性传递，盒装收益小于可读性代价
pub enum Event {
    Status(String),
    Error(String),
    Folders(Vec<FolderInfo>),
    FolderLoaded {
        folder: String,
        rows: Vec<MessageRow>,
    },
    BodyLoaded {
        hash: i64,
        /// 图片附件（name, bytes）：UI 内联预览用；魔数过滤 + 2MB/张上限
        attachment_images: Vec<(String, Vec<u8>)>,
        /// PDF 附件缩略图（name, png_bytes）：QuickLook 生成，UI 预览用
        attachment_previews: Vec<(String, Vec<u8>)>,
        subject: String,
        /// 原信收件人（"名字 <a@b>, …" 原样，回复全部用）
        to_addrs: String,
        cc_addrs: String,
        /// List-Unsubscribe 退订入口（http/mailto），无则 None
        unsubscribe: Option<String>,
        /// 内嵌图片（cid；随邮件自带，自动显示）：(cid, 字节)
        inline_images: Vec<(String, Vec<u8>)>,
        /// 远程图片 URL（隐私默认不加载；用户点击后经 LoadImages 按需下载）
        remote_images: Vec<String>,
        /// M5-c：PGP 状态（None=非 PGP 邮件）
        pgp: Option<String>,
        body: String,
        /// true = body 是 markdown（源自 HTML 邮件，UI 用 markdown widget 渲染）
        is_html: bool,
        /// (文件名, 字节数)
        attachments: Vec<(String, usize)>,
    },
    MailSent {
        to: String,
        subject: String,
    },
    DraftsListed(Vec<DraftRow>),
    DraftSaved {
        id: i64,
    },
    DraftDeleted(i64),
    MessageDeleted(i64),
    FlagsChanged {
        hash: i64,
        seen: Option<bool>,
        flagged: Option<bool>,
    },
    /// 发件箱计数变化（侧栏入口显隐/刷新信号）
    OutboxPending {
        pending: usize,
        failed: usize,
    },
    /// 发件箱全量（发件箱视图）
    OutboxListed(Vec<OutboxRow>),
    /// 连接状态（true=已连接；断连时 UI 持续提示而非一闪而过的状态行）
    ConnState(bool),
    // M3
    SearchResults {
        query: String,
        rows: Vec<MessageRow>,
        /// 匹配正文片段（与 rows 同序；空=仅主题命中）
        snippets: Vec<String>,
    },
    /// 统一收件箱聚合行（LocalList 的回流；带账号标签）
    UnifiedRows {
        rows: Vec<MessageRow>,
    },
    /// 远程图片下载完成（按需加载；键回 hash）
    ImagesLoaded {
        hash: i64,
        images: Vec<(String, Vec<u8>)>,
    },
    /// M6：OAuth 设备流等待用户授权（GUI 显示链接+验证码横幅）
    OAuthPrompt {
        url: String,
        code: String,
    },
    AttachmentSaved {
        path: String,
    },
    // ---- M7 ----
    /// INBOX 增量新信（系统通知用；初次全量同步不发）
    NewMail {
        from: String,
        subject: String,
        /// 新邮件所在文件夹（通知跳转直达；空=未知）
        folder: String,
        /// 新邮件 hash（通知跳转直达）
        hash: i64,
    },
    /// 最近联系人（Cmd::ListContacts 的响应）
    /// 通讯录补全数据（compose 用，display 格式 "名字 <a@b>"）
    ContactsListed(Vec<String>),
    /// 通讯录全量（管理界面用）
    ContactsFull(Vec<ContactRow>),
    /// 同步进度（文件夹同步中发送，UI 显示「正在同步 INBOX (120/500)」进度条）
    SyncProgress {
        /// 正在同步的文件夹名
        folder: String,
        /// 总邮件数
        total: usize,
        /// 已处理数
        processed: usize,
    },
    /// 存量邮件预览回填完成（后台线程从本地 bodies 缓存生成）：
    /// UI 收到后重读当前文件夹，列表第三行预览立即出现
    PreviewsBackfilled {
        /// 本次回填的邮件数
        count: usize,
    },
}
