//! 文件传输应用（p2p_app 应用层）：通过 L1 frame 通道的 `file.*` 语义标签在已互信联系人间传文件。
//!
//! 复用语义注册表机制：注册 `file.offer/accept/reject/chunk/ack/finish/complete/abort`
//! 标签 + async handler，不动核心。发送侧为**事件驱动推送**（accept/ack 事件触发下一块
//! 发送，不用后台任务）；接收侧逐块写盘、完成时校验 sha256 并改名。

use colored::Colorize;
use libp2p::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

use crate::p2p_app::chat::handlers::AppCtx;
use crate::p2p::seam;
use crate::p2p::settings;

/// 文件分块大小（1 MiB）
const CHUNK_SIZE: usize = 1024 * 1024;

pub const TAG_FILE_OFFER: &str = "file.offer";
pub const TAG_FILE_ACCEPT: &str = "file.accept";
pub const TAG_FILE_REJECT: &str = "file.reject";
pub const TAG_FILE_CHUNK: &str = "file.chunk";
pub const TAG_FILE_ACK: &str = "file.ack";
pub const TAG_FILE_FINISH: &str = "file.finish";
pub const TAG_FILE_COMPLETE: &str = "file.complete";
pub const TAG_FILE_ABORT: &str = "file.abort";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOfferPayload {
    pub file_id: u64,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAcceptPayload {
    pub file_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRejectPayload {
    pub file_id: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChunkPayload {
    pub file_id: u64,
    pub seq: u64,
    pub data: Vec<u8>,
    /// 分块数据 CRC32（IEEE），接收侧写盘前校验
    pub crc: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAckPayload {
    pub file_id: u64,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFinishPayload {
    pub file_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCompletePayload {
    pub file_id: u64,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileAbortPayload {
    pub file_id: u64,
    pub reason: String,
}

/// offer 待决数据（phase1 登记 → phase2 `complete_file_receive` 所需完整信息；
/// 会话层据此登记 pending_confirm 并按模式拉起确认子窗口）
pub struct FilePending {
    pub from: PeerId,
    pub file_id: u64,
    pub name: String,
    pub size: u64,
}

/// 接收中的文件（逐块写盘——每块经 CRC32 校验；完成时改名落盘）
struct ReceivingFile {
    file: tokio::fs::File,
    tmp_path: PathBuf,
    written: u64,
    name: String,
    /// offer 声明的总字节（接收进度百分比）
    size: u64,
    /// 已收块数（GUI 进度节流：每 8 块/尾块发一次事件）
    chunks_received: u64,
}

/// offer 等待确认超时：超时未收到 file.accept 则中止发送并清理状态（防双方悬挂）
pub(crate) const OFFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// 发送中的文件（事件驱动推送：accept/ack 事件触发下一块发送，不用后台任务）
struct SendingFile {
    peer: PeerId,
    file: std::fs::File,
    size: u64,
    name: String,
    next_seq: u64,
    sent: u64,
    /// offer 发出时刻（sent==0 期间超时未确认 → 中止；accept 后首块推送即离开等待期）
    offer_at: std::time::Instant,
}

/// 文件传输应用状态（应用任务内）：发送状态表 + 接收文件表 + 文件 id 计数器
pub struct FileTransferState {
    senders: HashMap<u64, SendingFile>,
    receivers: HashMap<(PeerId, u64), ReceivingFile>,
    next_id: u64,
    downloads_dir: PathBuf,
}

impl FileTransferState {
    /// 解析下载目录并尽量提前创建（失败不致命，接收时会再次尝试并报错）。
    /// 优先级：P2P_DOWNLOAD_DIR 环境变量 → 设置文件 download_dir → 用户主目录 Downloads → ./downloads
    pub fn new(peer_id: &PeerId) -> Self {
        let downloads_dir = resolve_download_dir(peer_id);
        let _ = std::fs::create_dir_all(&downloads_dir);
        FileTransferState {
            senders: HashMap::new(),
            receivers: HashMap::new(),
            next_id: 1,
            downloads_dir,
        }
    }

    /// 当前下载目录（解析后的绝对/相对路径）
    pub fn downloads_dir(&self) -> &Path {
        &self.downloads_dir
    }

    /// 运行时更新下载目录（设置页/命令落账后调用；立即生效——后续接收建临时文件用新目录，
    /// 进行中传输的改名仍用接收时目录）。尽量提前创建，失败不致命（接收时会再试并报错）。
    pub fn set_downloads_dir(&mut self, dir: PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        self.downloads_dir = dir;
    }

    /// 最近的未确认 offer 过期时刻（sent==0；无等待中的 offer 返回 None）
    /// —— 会话主循环定时臂据此 sleep_until
    pub fn next_offer_expiry(&self) -> Option<std::time::Instant> {
        self.senders
            .values()
            .filter(|s| s.sent == 0)
            .map(|s| s.offer_at + OFFER_TIMEOUT)
            .min()
    }

    /// 清理过期的未确认 offer（sent==0 且超时）：移除发送态并返回待通知的 (peer, file_id, name)。
    /// 由会话主循环定时臂调用——对端不应答（网络突发/对端退出）时防止状态悬挂。
    pub fn expire_stale_offers(
        &mut self,
        timeout: std::time::Duration,
    ) -> Vec<(PeerId, u64, String)> {
        let stale: Vec<u64> = self
            .senders
            .iter()
            .filter(|(_, s)| s.sent == 0 && s.offer_at.elapsed() >= timeout)
            .map(|(id, _)| *id)
            .collect();
        let mut out = Vec::new();
        for id in stale {
            if let Some(s) = self.senders.remove(&id) {
                out.push((s.peer, id, s.name));
            }
        }
        out
    }
}

/// 解析下载目录：环境变量优先，其次 per-identity 设置，然后用户主目录 Downloads，最后兜底
fn resolve_download_dir(peer_id: &PeerId) -> PathBuf {
    let configured = std::env::var("P2P_DOWNLOAD_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .or_else(|| settings::load_download_dir(peer_id))
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from);
    configured
        .or_else(default_downloads_dir)
        .unwrap_or_else(|| PathBuf::from("downloads"))
}

/// 用户主目录的 Downloads 目录（Windows: %USERPROFILE%\Downloads，Unix: $HOME/Downloads）
fn default_downloads_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(|h| PathBuf::from(h).join("Downloads"))
}

/// 文件名净化：只取 basename，拒绝路径穿越
fn sanitize_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    if base.is_empty() || base == "." || base == ".." {
        "unnamed".to_string()
    } else {
        base.to_string()
    }
}

// ---- 接收侧 handler（注册到 SignalRegistry）----

/// 构造传输视图并经显示路由发出（GUI 传输卡片数据源）。
/// CLI 文案由调用方以 `cli_text` 预构造传入（逐字节保持 e2e 契约）；None = CLI 静默
/// （纯 GUI 事件，如接收进度/失败终态——历史 CLI 无此输出）。
fn emit_view(
    ctx: &AppCtx<'_>,
    peer: &PeerId,
    file_id: u64,
    name: String,
    outgoing: bool,
    total: u64,
    sent: u64,
    done: bool,
    ok: bool,
    saved_path: Option<String>,
    error: Option<String>,
    cli_text: Option<String>,
) {
    let view = crate::uievent::FileTransferView {
        peer: peer.to_string(),
        file_id,
        peer_name: crate::p2p_app::chat::ctx::peer_name(peer, ctx.conversations, ctx.identity),
        name,
        outgoing,
        total,
        sent,
        done,
        ok,
        saved_path,
        error,
    };
    crate::p2p_app::chat::display::file_transfer(view, cli_text);
}

/// 收到文件 offer（phase1，**不阻塞会话循环**）：三态——
/// - Interactive：打印提示 → 登记 `ctx.file_pending`（会话层拉起 `--confirm-file` 子窗口；
///   非 Windows 退化为主窗口 pending 路由）
/// - Ask：发系统消息卡片 → 登记 `ctx.file_pending`（答案经 InputMsg::Line 回程）
/// - Auto：直接进入 phase2 接受（e2e/脚本语义不变）
/// 接受/拒绝落账（建临时文件/回 accept/reject）在 phase2 [`complete_file_receive`]。
pub async fn on_file_offer(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileOfferPayload>(bytes) else {
        return false;
    };
    let name = sanitize_file_name(&p.name);
    match ctx.mode {
        crate::lineio::ConfirmMode::Auto => {
            complete_file_receive(ctx, from, p.file_id, name, p.size, true).await;
        }
        crate::lineio::ConfirmMode::Interactive => {
            println!(
                "{}",
                format!(
                    "收到文件: {name}（{} 字节，来自 {from}），请在确认子窗口选择是否保存\
                     （任务栏可见；主窗口聊天不受影响）",
                    p.size
                )
                    .yellow()
            );
            ctx.file_pending = Some(FilePending {
                from: *from,
                file_id: p.file_id,
                name,
                size: p.size,
            });
        }
        crate::lineio::ConfirmMode::Ask => {
            // 设置开关：confirm_file_receive=off → 自动接收（信任伙伴免每次点卡片）；
            // on（默认）→ 系统消息卡片。CLI Interactive 逐次 y/n 与 Auto 自动接受不受此开关控制。
            if !crate::p2p::settings::load_confirm_file_receive(ctx.identity.my_id()) {
                complete_file_receive(ctx, from, p.file_id, name, p.size, true).await;
                return true;
            }
            // GUI：发系统消息卡片（答案经 InputMsg::AskAnswer 专道）
            crate::sink::ask(crate::uievent::AskRequest {
                id: crate::uievent::next_ask_id(),
                kind: crate::uievent::AskKind::FileReceive {
                    from: from.to_string(),
                    filename: name.clone(),
                    size: p.size,
                },
                secret: false,
            });
            ctx.file_pending = Some(FilePending {
                from: *from,
                file_id: p.file_id,
                name,
                size: p.size,
            });
        }
    }
    true
}

/// offer 确认 phase2：accept=true 建临时文件并回 file.accept（开始接收）；
/// false 回 file.reject。原 on_file_offer 的接受/拒绝落账路径整体迁出——
/// 两段式后确认等待不再内联阻塞会话循环，答案到达（子窗口/卡片/主窗口行）时进入本函数。
pub async fn complete_file_receive(
    ctx: &mut AppCtx<'_>,
    from: &PeerId,
    file_id: u64,
    name: String,
    size: u64,
    accept: bool,
) {
    if accept {
        let dir = ctx.file.downloads_dir.clone();
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            eprintln!(
                "{}",
                format!("创建下载目录失败: {e}（路径: {}）", dir.display()).yellow()
            );
            let _ = send_signal(ctx, from, TAG_FILE_REJECT, &FileRejectPayload {
                file_id,
                reason: format!("本地创建下载目录失败: {e}"),
            })
            .await;
            return;
        }
        let tmp_path = dir.join(format!(".{name}.part.{file_id}"));
        let file = match tokio::fs::File::create(&tmp_path).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("创建接收文件失败: {e}（路径: {}）", tmp_path.display()).yellow()
                );
                let _ = send_signal(ctx, from, TAG_FILE_REJECT, &FileRejectPayload {
                    file_id,
                    reason: format!("本地创建接收文件失败: {e}"),
                })
                .await;
                return;
            }
        };
        ctx.file.receivers.insert(
            (*from, file_id),
            ReceivingFile {
                file,
                tmp_path,
                written: 0,
                name: name.clone(),
                size,
                chunks_received: 0,
            },
        );
        let _ = send_signal(ctx, from, TAG_FILE_ACCEPT, &FileAcceptPayload { file_id }).await;
        emit_view(
            ctx,
            from,
            file_id,
            name.clone(),
            false,
            size,
            0,
            false,
            false,
            None,
            None,
            Some(format!("开始接收 {name}（{size} 字节）...").green().to_string()),
        );
    } else {
        let _ = send_signal(ctx, from, TAG_FILE_REJECT, &FileRejectPayload {
            file_id,
            reason: "对方拒绝接收".into(),
        })
        .await;
        println!("{}", format!("已拒绝接收 {name}").dimmed());
    }
}

/// 收到文件分块：先校验 CRC32，通过才写盘并回 file.ack（发送侧逐块推进）
pub async fn on_file_chunk(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileChunkPayload>(bytes) else {
        return false;
    };
    let key = (*from, p.file_id);
    let Some(r) = ctx.file.receivers.get_mut(&key) else {
        return false;
    };
    if crc32fast::hash(&p.data) != p.crc {
        // 分块校验失败：删临时文件、移除接收状态、通知对端中止（卡片终态；CLI 历史无输出）
        let (name, size, written, tmp_path) =
            (r.name.clone(), r.size, r.written, r.tmp_path.clone());
        ctx.file.receivers.remove(&key);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        let reason = format!("分块校验失败 seq={}", p.seq);
        let _ = send_signal(ctx, from, TAG_FILE_ABORT, &FileAbortPayload {
            file_id: p.file_id,
            reason: reason.clone(),
        })
        .await;
        emit_view(
            ctx, from, p.file_id, name, false, size, written, true, false, None, Some(reason), None,
        );
        return true;
    }
    if let Err(e) = r.file.write_all(&p.data).await {
        let reason = format!("写盘失败: {e}");
        let (name, size, written) = (r.name.clone(), r.size, r.written);
        let _ = send_signal(ctx, from, TAG_FILE_ABORT, &FileAbortPayload {
            file_id: p.file_id,
            reason: reason.clone(),
        })
        .await;
        ctx.file.receivers.remove(&key);
        emit_view(
            ctx, from, p.file_id, name, false, size, written, true, false, None, Some(reason), None,
        );
        return true;
    }
    r.written += p.data.len() as u64;
    r.chunks_received += 1;
    let throttle = r.chunks_received % 8 == 0 || r.written >= r.size;
    let (written, size, name) = (r.written, r.size, r.name.clone());
    let _ = send_signal(ctx, from, TAG_FILE_ACK, &FileAckPayload {
        file_id: p.file_id,
        seq: p.seq,
    })
    .await;
    // 接收进度（历史 CLI 无此输出——纯 GUI 事件，cli_text=None）
    if throttle {
        emit_view(ctx, from, p.file_id, name, false, size, written, false, false, None, None, None);
    }
    true
}

/// 收到 file.finish：flush、临时文件改名、回 file.complete
pub async fn on_file_finish(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileFinishPayload>(bytes) else {
        return false;
    };
    let key = (*from, p.file_id);
    let Some(r) = ctx.file.receivers.remove(&key) else {
        return false;
    };
    let (name, size, written) = (r.name.clone(), r.size, r.written);
    // flush + 改名
    let mut file = r.file;
    let _ = file.flush().await;
    let tmp_path = r.tmp_path;
    let final_path = ctx.file.downloads_dir.join(&r.name);
    if let Err(e) = tokio::fs::rename(&tmp_path, &final_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        let reason = format!("接收文件改名失败: {e}");
        let _ = send_signal(ctx, from, TAG_FILE_COMPLETE, &FileCompletePayload {
            file_id: p.file_id,
            ok: false,
            error: Some(reason.clone()),
        })
        .await;
        emit_view(
            ctx, from, p.file_id, name, false, size, written, true, false, None, Some(reason), None,
        );
        return true;
    }
    let _ = send_signal(ctx, from, TAG_FILE_COMPLETE, &FileCompletePayload {
        file_id: p.file_id,
        ok: true,
        error: None,
    })
    .await;
    emit_view(
        ctx,
        from,
        p.file_id,
        name,
        false,
        size,
        written.max(size),
        true,
        true,
        Some(final_path.display().to_string()),
        None,
        Some(format!("文件接收完成: {}", final_path.display()).green().to_string()),
    );
    let _ = file;
    true
}

/// 收到 file.abort：若在发送则移除发送状态；若在接收则清理临时文件
pub async fn on_file_abort(ctx: &mut AppCtx<'_>, from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileAbortPayload>(bytes) else {
        return false;
    };
    if let Some(s) = ctx.file.senders.remove(&p.file_id) {
        emit_view(
            ctx,
            &s.peer,
            p.file_id,
            s.name.clone(),
            true,
            s.size,
            s.sent,
            true,
            false,
            None,
            Some(p.reason.clone()),
            Some(format!("文件发送中止: {}", p.reason).yellow().to_string()),
        );
    }
    if let Some(r) = ctx.file.receivers.remove(&(*from, p.file_id)) {
        let _ = tokio::fs::remove_file(&r.tmp_path).await;
        emit_view(
            ctx,
            from,
            p.file_id,
            r.name.clone(),
            false,
            r.size,
            r.written,
            true,
            false,
            None,
            Some(p.reason.clone()),
            Some(format!("文件接收中止: {}", p.reason).yellow().to_string()),
        );
    }
    true
}

// ---- 发送侧：事件驱动推送（accept/ack 事件触发下一块发送，不用后台任务）----

pub async fn on_file_accept(ctx: &mut AppCtx<'_>, _from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileAcceptPayload>(bytes) else {
        return false;
    };
    if !ctx.file.senders.contains_key(&p.file_id) {
        // 迟到的 accept（发送侧 offer 已超时清理）：回 abort 让对端清理悬挂的接收态
        let _ = send_signal(
            ctx,
            _from,
            TAG_FILE_ABORT,
            &FileAbortPayload {
                file_id: p.file_id,
                reason: "offer 已超时失效".into(),
            },
        )
        .await;
        return true;
    }
    drive_sender(ctx, p.file_id).await;
    true
}

pub async fn on_file_reject(ctx: &mut AppCtx<'_>, _from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileRejectPayload>(bytes) else {
        return false;
    };
    if let Some(s) = ctx.file.senders.remove(&p.file_id) {
        emit_view(
            ctx,
            &s.peer,
            p.file_id,
            s.name.clone(),
            true,
            s.size,
            s.sent,
            true,
            false,
            None,
            Some(p.reason.clone()),
            Some(format!("对方拒绝接收: {}", p.reason).yellow().to_string()),
        );
    }
    true
}

pub async fn on_file_ack(ctx: &mut AppCtx<'_>, _from: &PeerId, payload: Option<&[u8]>) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileAckPayload>(bytes) else {
        return false;
    };
    drive_sender(ctx, p.file_id).await;
    true
}

pub async fn on_file_complete(
    ctx: &mut AppCtx<'_>,
    _from: &PeerId,
    payload: Option<&[u8]>,
) -> bool {
    let Some(bytes) = payload else {
        return false;
    };
    let Ok(p) = serde_cbor::from_slice::<FileCompletePayload>(bytes) else {
        return false;
    };
    if let Some(s) = ctx.file.senders.remove(&p.file_id) {
        if p.ok {
            emit_view(
                ctx,
                &s.peer,
                p.file_id,
                s.name.clone(),
                true,
                s.size,
                s.size,
                true,
                true,
                None,
                None,
                Some(format!("文件发送完成: {}", s.name).green().to_string()),
            );
        } else {
            let reason = p.error.unwrap_or_default();
            emit_view(
                ctx,
                &s.peer,
                p.file_id,
                s.name.clone(),
                true,
                s.size,
                s.sent,
                true,
                false,
                None,
                Some(reason.clone()),
                Some(format!("接收方校验失败: {reason}").yellow().to_string()),
            );
        }
    }
    true
}

/// 事件驱动推送：读下一块并发送（读完发 finish）。由 accept/ack 事件触发。
async fn drive_sender(ctx: &mut AppCtx<'_>, file_id: u64) {
    let Some(peer) = ctx.file.senders.get(&file_id).map(|s| s.peer) else {
        return;
    };
    let (s_name, s_size) = match ctx.file.senders.get(&file_id) {
        Some(s) => (s.name.clone(), s.size),
        None => return,
    };
    // 读下一块（或读完发 finish；读失败发 abort 并移除发送状态）
    let result: Option<(String, Option<Vec<u8>>, Option<(u64, u64, u64)>, Option<String>)> = {
        let Some(s) = ctx.file.senders.get_mut(&file_id) else {
            return;
        };
        let mut buf = vec![0u8; CHUNK_SIZE];
        match s.file.read(&mut buf) {
            Ok(0) => Some((
                TAG_FILE_FINISH.to_string(),
                Some(serde_cbor::to_vec(&FileFinishPayload { file_id }).unwrap_or_default()),
                None,
                None,
            )),
            Ok(n) => {
                let chunk = (
                    TAG_FILE_CHUNK.to_string(),
                    Some(
                        serde_cbor::to_vec(&FileChunkPayload {
                            file_id,
                            seq: s.next_seq,
                            data: buf[..n].to_vec(),
                            crc: crc32fast::hash(&buf[..n]),
                        })
                        .unwrap_or_default(),
                    ),
                );
                let progress = Some((s.sent + n as u64, s.size, s.next_seq));
                s.sent += n as u64;
                s.next_seq += 1;
                Some((chunk.0, chunk.1, progress, None))
            }
            Err(e) => {
                let reason = format!("读文件失败: {e}");
                ctx.file.senders.remove(&file_id);
                Some((
                    TAG_FILE_ABORT.to_string(),
                    Some(
                        serde_cbor::to_vec(&FileAbortPayload { file_id, reason: reason.clone() })
                            .unwrap_or_default(),
                    ),
                    None,
                    Some(reason),
                ))
            }
        }
    };
    let Some((tag, payload, progress, error)) = result else {
        return;
    };
    let _ = ctx
        .cmd_tx
        .send(seam::Cmd::Send {
            peer,
            tag,
            payload,
        })
        .await;
    if let Some((sent, size, seq)) = progress {
        let total = if size == 0 {
            0
        } else {
            (size + CHUNK_SIZE as u64 - 1) / CHUNK_SIZE as u64
        };
        if seq % 8 == 0 || seq == total {
            let pct = if size == 0 { 100 } else { (sent * 100 / size) as u8 };
            emit_view(
                ctx,
                &peer,
                file_id,
                s_name.clone(),
                true,
                size,
                sent,
                false,
                false,
                None,
                None,
                Some(format!("已发送 {sent}/{size} 字节（{pct}%）").dimmed().to_string()),
            );
        }
    }
    // 读失败：发送卡片终态（CLI 历史无此输出——纯 GUI 事件）
    if let Some(reason) = error {
        emit_view(
            ctx,
            &peer,
            file_id,
            s_name,
            true,
            s_size,
            0,
            true,
            false,
            None,
            Some(reason),
            None,
        );
    }
}

/// 构造 file.* 帧并发给对端
async fn send_signal<T: Serialize>(
    ctx: &mut AppCtx<'_>,
    peer: &PeerId,
    tag: &str,
    payload: &T,
) -> bool {
    let bin = match serde_cbor::to_vec(payload) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let _ = ctx
        .cmd_tx
        .send(seam::Cmd::Send {
            peer: *peer,
            tag: tag.to_string(),
            payload: Some(bin),
        })
        .await;
    true
}

/// 发送文件：/send 命令与 GUI Control::SendFile 调用（同步）。登记发送状态、把 offer
/// 排入命令队列；后续块由 accept/ack 事件驱动推送（drive_sender），不用后台任务。
/// `peer_name` 为对端显示名（GUI 传输卡片展示；CLI 文案仍为节点 ID，逐字节保持）。
pub fn start_send(
    state: &mut FileTransferState,
    ops: &mut VecDeque<crate::p2p_app::chat::ctx::AsyncOp>,
    peer: PeerId,
    path: &Path,
    peer_name: &str,
) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("读取文件失败: {e}"))?;
    if !meta.is_file() {
        return Err("目标不是普通文件".into());
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string();
    let size = meta.len();
    let file_id = state.next_id;
    state.next_id += 1;
    let file = std::fs::File::open(path).map_err(|e| format!("打开文件失败: {e}"))?;
    state.senders.insert(
        file_id,
        SendingFile {
            peer,
            file,
            size,
            name: name.clone(),
            next_seq: 0,
            sent: 0,
            offer_at: std::time::Instant::now(),
        },
    );
    let offer = serde_cbor::to_vec(&FileOfferPayload {
        file_id,
        name: name.clone(),
        size,
    })
    .map_err(|e| format!("序列化失败: {e}"))?;
    ops.push_back(crate::p2p_app::chat::ctx::AsyncOp::Cmd(seam::Cmd::Send {
        peer,
        tag: TAG_FILE_OFFER.to_string(),
        payload: Some(offer),
    }));
    let view = crate::uievent::FileTransferView {
        peer: peer.to_string(),
        file_id,
        peer_name: peer_name.to_string(),
        name: name.clone(),
        outgoing: true,
        total: size,
        sent: 0,
        done: false,
        ok: false,
        saved_path: None,
        error: None,
    };
    crate::p2p_app::chat::display::file_transfer(
        view,
        Some(
            format!("开始发送 {name}（{size} 字节）给 {peer}（等待对方确认）...")
                .green()
                .to_string(),
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_drops_path_and_blocks_traversal() {
        assert_eq!(sanitize_file_name("a/b/c.bin"), "c.bin");
        assert_eq!(sanitize_file_name(r"C:\dir\file.txt"), "file.txt");
        assert_eq!(sanitize_file_name("..\\..\\evil.sh"), "evil.sh");
        assert_eq!(sanitize_file_name(".."), "unnamed");
        assert_eq!(sanitize_file_name(""), "unnamed");
        assert_eq!(sanitize_file_name("normal.bin"), "normal.bin");
    }

    #[test]
    fn crc32_known_vector() {
        // CRC32-IEEE 标准向量："123456789" → 0xCBF43926
        assert_eq!(crc32fast::hash(b"123456789"), 0xCBF43926);
        // 不同数据 CRC 不同（校验能区分损坏块）
        assert_ne!(crc32fast::hash(b"hello"), crc32fast::hash(b"hellp"));
    }

    #[test]
    fn expire_stale_offers_only_unconfirmed_and_expired() {
        use std::time::{Duration, Instant};
        let peer: PeerId = "12D3KooWGpERtoeJ1M482Kkx7p9czC9yKYuXGsvUvDBG3589iPKq"
            .parse()
            .unwrap();
        let mut st = FileTransferState::new(&peer);
        let path = std::env::temp_dir().join("p2p_ft_expire_test.txt");
        std::fs::write(&path, b"hello").unwrap();

        // 已确认（sent>0）：不清理
        st.next_id += 1;
        st.senders.insert(
            1,
            SendingFile {
                peer,
                file: std::fs::File::open(&path).unwrap(),
                size: 5,
                name: "a.txt".into(),
                next_seq: 1,
                sent: 3,
                offer_at: Instant::now() - Duration::from_secs(3600),
            },
        );
        // 过期未确认：清理
        st.senders.insert(
            2,
            SendingFile {
                peer,
                file: std::fs::File::open(&path).unwrap(),
                size: 5,
                name: "b.txt".into(),
                next_seq: 0,
                sent: 0,
                offer_at: Instant::now() - Duration::from_secs(120),
            },
        );
        // 新鲜未确认：不清理
        st.senders.insert(
            3,
            SendingFile {
                peer,
                file: std::fs::File::open(&path).unwrap(),
                size: 5,
                name: "c.txt".into(),
                next_seq: 0,
                sent: 0,
                offer_at: Instant::now(),
            },
        );

        // 最近的过期时刻 = 过期条目的 offer_at + 60s
        assert!(st.next_offer_expiry().is_some());

        let expired = st.expire_stale_offers(Duration::from_secs(60));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].0, peer);
        assert_eq!(expired[0].1, 2);
        assert_eq!(expired[0].2, "b.txt");
        assert!(st.senders.contains_key(&1), "已确认的不清理");
        assert!(st.senders.contains_key(&3), "新鲜的 不清理");
        assert!(!st.senders.contains_key(&2));
        std::fs::remove_file(&path).ok();
    }
}
