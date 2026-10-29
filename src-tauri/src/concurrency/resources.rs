//! 资源锁表：粗粒度资源（会话、时间线、内容写、元数据写、AI 服务）与文件夹子树路径锁。
//!
//! 所有资源一次性申请：全部可用才同时占用，否则一个都不占。等待者按“与该请求冲突的
//! 既有等待者”排队，这同时实现了路径锁的写者优先——文件夹写者等待时，新读者不再插队。
//!
//! 排队中的任务一个字节都没写，所以它只在**路径锁**上占用优先权：短写（评分、标签、
//! 集合）立即申请时不看排队者，不会被“排队等 AI 的描述任务”挡住；路径写者优先保留，
//! 否则一个文件夹写者会被源源不断的读者饿死。

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    /// 原地替换原有内容：发布走“临时文件 + 原子重命名”，读者只会看到旧内容或新内容，
    /// 所以不与读互斥；与移动、删除、导入这些会改变路径的写仍然互斥。
    ///
    /// 读者正持着句柄也能替换：Rust 的 `File::open` 带 `FILE_SHARE_DELETE`，
    /// Windows 的重命名会成功，旧文件在最后一个句柄关闭后才真正消失。
    Replace,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathScope {
    pub path: String,
    pub access: Access,
}

impl PathScope {
    pub fn read(path: impl AsRef<str>) -> Self {
        Self {
            path: normalize_path(path.as_ref()),
            access: Access::Read,
        }
    }

    /// 原地替换图片内容，不改变文件夹结构与文件名。
    pub fn replace(path: impl AsRef<str>) -> Self {
        Self {
            path: normalize_path(path.as_ref()),
            access: Access::Replace,
        }
    }

    pub fn write(path: impl AsRef<str>) -> Self {
        Self {
            path: normalize_path(path.as_ref()),
            access: Access::Write,
        }
    }
}

/// 持有者。`id` 用于同一任务追加短读锁时豁免自身；错误文案只用 `label` 与 `reason`。
#[derive(Debug, Clone)]
pub struct HolderInfo {
    pub id: String,
    pub label: String,
    pub reason: String,
    /// 路径冲突时的动词，例如“正在导入到”；为空时按读/写自动生成。
    pub path_note: Option<&'static str>,
}

impl HolderInfo {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            reason: "正在进行".to_owned(),
            path_note: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    pub fn with_path_note(mut self, note: &'static str) -> Self {
        self.path_note = Some(note);
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResourceRequest {
    pub session_read: bool,
    pub session_write: bool,
    pub timeline: Option<Access>,
    pub content_writer: bool,
    pub metadata_writer: bool,
    pub ai_service: bool,
    pub paths: Vec<PathScope>,
    pub path_note: Option<&'static str>,
}

impl ResourceRequest {
    pub fn session_read() -> Self {
        Self {
            session_read: true,
            ..Self::default()
        }
    }

    pub fn session_write() -> Self {
        Self {
            session_write: true,
            ..Self::default()
        }
    }

    pub fn path_read(path: impl AsRef<str>) -> Self {
        Self {
            session_read: true,
            paths: vec![PathScope::read(path)],
            ..Self::default()
        }
    }

    pub fn path_write(path: impl AsRef<str>) -> Self {
        Self {
            session_read: true,
            paths: vec![PathScope::write(path)],
            ..Self::default()
        }
    }

    pub fn path_replace(path: impl AsRef<str>) -> Self {
        Self {
            session_read: true,
            paths: vec![PathScope::replace(path)],
            ..Self::default()
        }
    }

    pub fn with_timeline(mut self, access: Access) -> Self {
        self.timeline = Some(access);
        self
    }

    pub fn with_content_writer(mut self) -> Self {
        self.content_writer = true;
        self
    }

    pub fn with_metadata_writer(mut self) -> Self {
        self.metadata_writer = true;
        self
    }

    pub fn with_ai_service(mut self) -> Self {
        self.ai_service = true;
        self
    }

    pub fn with_paths(mut self, paths: Vec<PathScope>) -> Self {
        self.paths = paths;
        self
    }

    pub fn with_path_note(mut self, note: &'static str) -> Self {
        self.path_note = Some(note);
        self
    }

    pub fn is_empty(&self) -> bool {
        !self.session_read
            && !self.session_write
            && self.timeline.is_none()
            && !self.content_writer
            && !self.metadata_writer
            && !self.ai_service
            && self.paths.is_empty()
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        // 会话写与任何非空请求互斥；空请求（无资源）不挡换库。
        if self.session_write || other.session_write {
            return !self.is_empty() && !other.is_empty();
        }
        if timeline_conflicts(self.timeline, other.timeline) {
            return true;
        }
        if self.content_writer && other.content_writer {
            return true;
        }
        if self.metadata_writer && other.metadata_writer {
            return true;
        }
        if self.ai_service && other.ai_service {
            return true;
        }
        self.paths.iter().any(|left| {
            other
                .paths
                .iter()
                .any(|right| path_scopes_conflict(left, right))
        })
    }

    fn conflicting_path<'a>(&'a self, other: &'a Self) -> Option<(&'a PathScope, &'a PathScope)> {
        for left in &self.paths {
            for right in &other.paths {
                if path_scopes_conflict(left, right) {
                    return Some((left, right));
                }
            }
        }
        None
    }
}

fn timeline_conflicts(left: Option<Access>, right: Option<Access>) -> bool {
    match (left, right) {
        (Some(Access::Read), Some(Access::Read)) | (None, _) | (_, None) => false,
        (Some(_), Some(_)) => true,
    }
}

fn path_scopes_conflict(left: &PathScope, right: &PathScope) -> bool {
    if !accesses_conflict(left.access, right.access) {
        return false;
    }
    is_prefix(&left.path, &right.path) || is_prefix(&right.path, &left.path)
}

/// 访问级之间的互斥：读与读、读与原地替换都不互斥，其余都互斥。
fn accesses_conflict(left: Access, right: Access) -> bool {
    !matches!(
        (left, right),
        (Access::Read, Access::Read)
            | (Access::Read, Access::Replace)
            | (Access::Replace, Access::Read)
    )
}

fn is_prefix(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// 统一路径键：`/` 分隔、去首尾 `/`、小写；根为 `""`。
pub fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
        .to_lowercase()
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub holder: HolderInfo,
    pub detail: String,
    /// 占用者自己还没开始（只是排着队），文案要说“排队中”而不是“正在进行”。
    pub queued: bool,
}

impl Conflict {
    /// “「导入」正在写入「角色」文件夹”这样的短句，调用方自行补“请……”。
    pub fn sentence(&self) -> String {
        if self.queued {
            return format!("「{}」还在排队、尚未开始", self.holder.label);
        }
        if self.detail.is_empty() {
            format!("「{}」{}", self.holder.label, self.holder.reason)
        } else {
            format!("「{}」{}", self.holder.label, self.detail)
        }
    }
}

#[derive(Debug)]
pub enum WaitError {
    Cancelled,
    Timeout(Vec<Conflict>),
}

struct Active {
    seq: u64,
    holder: HolderInfo,
    request: ResourceRequest,
}

struct Waiter {
    seq: u64,
    holder: HolderInfo,
    request: ResourceRequest,
}

#[derive(Default)]
struct TableState {
    active: Vec<Active>,
    waiters: VecDeque<Waiter>,
}

struct Inner {
    state: Mutex<TableState>,
    ready: Condvar,
    next_seq: AtomicU64,
}

impl Inner {
    fn next_seq(&self) -> u64 {
        self.next_seq.fetch_add(1, Ordering::Relaxed)
    }
}

pub struct ResourceTable {
    inner: Arc<Inner>,
}

impl Default for ResourceTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceTable {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(TableState::default()),
                ready: Condvar::new(),
                next_seq: AtomicU64::new(1),
            }),
        }
    }

    pub fn try_acquire(
        &self,
        request: &ResourceRequest,
        holder: &HolderInfo,
    ) -> Result<ResourceGuard, Vec<Conflict>> {
        let mut state = self.inner.state.lock();
        let conflicts = state.conflicts(None, &holder.id, request);
        if !conflicts.is_empty() {
            return Err(conflicts);
        }
        Ok(self.acquire_locked(&mut state, request.clone(), holder.clone()))
    }

    /// 等待资源；每 200 ms 醒一次检查取消与超时。拿到锁之前不占用任何资源。
    pub fn acquire(
        &self,
        request: &ResourceRequest,
        holder: &HolderInfo,
        deadline: Option<Instant>,
        cancel: &dyn Fn() -> bool,
    ) -> Result<ResourceGuard, WaitError> {
        let mut state = self.inner.state.lock();
        let conflicts = state.conflicts(None, &holder.id, request);
        if conflicts.is_empty() {
            return Ok(self.acquire_locked(&mut state, request.clone(), holder.clone()));
        }

        let waiter_seq = self.inner.next_seq();
        state.waiters.push_back(Waiter {
            seq: waiter_seq,
            holder: holder.clone(),
            request: request.clone(),
        });

        loop {
            if cancel() {
                state.waiters.retain(|waiter| waiter.seq != waiter_seq);
                self.inner.ready.notify_all();
                return Err(WaitError::Cancelled);
            }
            let conflicts = state.conflicts(Some(waiter_seq), &holder.id, request);
            if conflicts.is_empty() {
                state.waiters.retain(|waiter| waiter.seq != waiter_seq);
                return Ok(self.acquire_locked(&mut state, request.clone(), holder.clone()));
            }
            if let Some(deadline) = deadline {
                let now = Instant::now();
                if now >= deadline {
                    state.waiters.retain(|waiter| waiter.seq != waiter_seq);
                    self.inner.ready.notify_all();
                    return Err(WaitError::Timeout(conflicts));
                }
                let wait = (deadline - now).min(Duration::from_millis(200));
                self.inner.ready.wait_for(&mut state, wait);
            } else {
                self.inner
                    .ready
                    .wait_for(&mut state, Duration::from_millis(200));
            }
            // 被外部取消时等待项可能已被移除，不能再按本地序号继续等。
            if !state.waiters.iter().any(|waiter| waiter.seq == waiter_seq) {
                return Err(WaitError::Cancelled);
            }
        }
    }

    pub fn acquire_path_read(
        &self,
        path: &str,
        holder: &HolderInfo,
        cancel: &dyn Fn() -> bool,
    ) -> Result<ResourceGuard, WaitError> {
        let request = ResourceRequest::path_read(path);
        self.acquire(&request, holder, None, cancel)
    }

    pub fn acquire_path_write(
        &self,
        path: &str,
        holder: &HolderInfo,
        cancel: &dyn Fn() -> bool,
    ) -> Result<ResourceGuard, WaitError> {
        let request = ResourceRequest::path_write(path);
        self.acquire(&request, holder, None, cancel)
    }

    pub fn remove_waiter(&self, holder_id: &str) {
        let mut state = self.inner.state.lock();
        state.waiters.retain(|waiter| waiter.holder.id != holder_id);
        self.inner.ready.notify_all();
    }

    pub fn is_idle(&self) -> bool {
        let state = self.inner.state.lock();
        state.active.is_empty() && state.waiters.is_empty()
    }

    #[cfg(test)]
    pub fn has_waiters(&self) -> bool {
        !self.inner.state.lock().waiters.is_empty()
    }

    fn acquire_locked(
        &self,
        state: &mut TableState,
        request: ResourceRequest,
        holder: HolderInfo,
    ) -> ResourceGuard {
        let seq = self.inner.next_seq();
        state.active.push(Active {
            seq,
            holder,
            request,
        });
        self.inner.ready.notify_all();
        ResourceGuard {
            table: self.inner.clone(),
            seq,
        }
    }
}

/// RAII 释放；只移除自己的条目并唤醒等待者，不回调业务代码。
pub struct ResourceGuard {
    table: Arc<Inner>,
    seq: u64,
}

impl Drop for ResourceGuard {
    fn drop(&mut self) {
        let mut state = self.table.state.lock();
        state.active.retain(|active| active.seq != self.seq);
        drop(state);
        self.table.ready.notify_all();
    }
}

impl TableState {
    fn conflicts(
        &self,
        own_waiter: Option<u64>,
        holder_id: &str,
        request: &ResourceRequest,
    ) -> Vec<Conflict> {
        let mut conflicts = Vec::new();
        for active in &self.active {
            if active.holder.id == holder_id {
                continue;
            }
            if request.conflicts_with(&active.request) {
                conflicts.push(describe(
                    &active.holder,
                    &active.request,
                    active
                        .request
                        .conflicting_path(request)
                        .map(|(held, _)| held),
                    false,
                ));
            }
        }
        for waiter in &self.waiters {
            if waiter.holder.id == holder_id {
                continue;
            }
            // 只让排在前面的等待者挡路：否则两个互斥的写者会互相等待。
            if let Some(own) = own_waiter
                && waiter.seq >= own
            {
                continue;
            }
            // 立即申请（`None`）虽然排在所有等待者之后，但等待者一个字节都还没写：
            // 只保留路径锁上的优先权，粗资源上的等待者不挡评分、标签这类短写。
            if own_waiter.is_none() && !shares_path_conflict(request, &waiter.request) {
                continue;
            }
            if request.conflicts_with(&waiter.request) {
                conflicts.push(describe(
                    &waiter.holder,
                    &waiter.request,
                    waiter
                        .request
                        .conflicting_path(request)
                        .map(|(held, _)| held),
                    true,
                ));
            }
        }
        conflicts
    }
}

fn shares_path_conflict(left: &ResourceRequest, right: &ResourceRequest) -> bool {
    left.paths.iter().any(|left| {
        right
            .paths
            .iter()
            .any(|right| path_scopes_conflict(left, right))
    })
}

fn describe(
    holder: &HolderInfo,
    holder_request: &ResourceRequest,
    holder_scope: Option<&PathScope>,
    queued: bool,
) -> Conflict {
    let detail = holder_scope.map_or_else(
        || holder.reason.clone(),
        |scope| match holder_request.path_note {
            Some(note) if scope.path.is_empty() => note.to_owned(),
            Some(note) => format!("{note}「{}」文件夹", scope.path),
            None if scope.access == Access::Read && !scope.path.is_empty() => {
                format!("正在读取「{}」中的图片", scope.path)
            }
            None if scope.access == Access::Replace && scope.path.is_empty() => {
                "正在替换图库中的原图".to_owned()
            }
            None if scope.access == Access::Replace => {
                format!("正在替换「{}」文件夹中的原图", scope.path)
            }
            None if scope.path.is_empty() => "正在处理图库内容".to_owned(),
            None => format!("正在写入「{}」文件夹", scope.path),
        },
    );
    Conflict {
        holder: holder.clone(),
        detail,
        queued,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn holder(id: &str) -> HolderInfo {
        HolderInfo::new(id, id)
    }

    fn read(path: &str) -> ResourceRequest {
        ResourceRequest::path_read(path)
    }

    fn write(path: &str) -> ResourceRequest {
        ResourceRequest::path_write(path)
    }

    #[test]
    fn path_rules_cover_subtrees_and_root() {
        let table = ResourceTable::new();
        let _root_read = table.try_acquire(&read(""), &holder("root")).unwrap();
        // 根读允许子孙读。
        let _child_read = table
            .try_acquire(&read("角色/a"), &holder("child"))
            .unwrap();
        // 根读挡住子孙写。
        assert!(
            table
                .try_acquire(&write("角色/a"), &holder("writer"))
                .is_err()
        );
        drop(_child_read);
        drop(_root_read);

        let _child = table
            .try_acquire(&write("角色/a"), &holder("child"))
            .unwrap();
        assert!(table.try_acquire(&read("角色"), &holder("parent")).is_err());
        assert!(
            table
                .try_acquire(&read("角色/a/b"), &holder("deep"))
                .is_err()
        );
        assert!(table.try_acquire(&read("其他"), &holder("other")).is_ok());
    }

    #[test]
    fn replace_lets_readers_through_but_blocks_writers() {
        let table = ResourceTable::new();
        // 全库原地替换：查看器与 MCP 读图照常。
        let held = table
            .try_acquire(&ResourceRequest::path_replace(""), &holder("optimize"))
            .unwrap();
        let viewer = table
            .try_acquire(&read("人体/素体"), &holder("viewer"))
            .unwrap();
        let _other = table
            .try_acquire(&read("画作"), &holder("viewer-2"))
            .unwrap();
        // 移动、导入、删除这类会改路径的写仍然挡住。
        assert!(
            table
                .try_acquire(&write("人体/素体"), &holder("move"))
                .is_err()
        );
        // 两个替换之间也互斥。
        assert!(
            table
                .try_acquire(&ResourceRequest::path_replace(""), &holder("optimize-2"))
                .is_err()
        );
        drop(viewer);
        drop(held);
        assert!(
            table
                .try_acquire(&write("人体/素体"), &holder("move"))
                .is_ok()
        );
    }

    #[test]
    fn multi_path_request_is_all_or_nothing() {
        let table = ResourceTable::new();
        let _held = table
            .try_acquire(&write("角色/a"), &holder("held"))
            .unwrap();
        let request = ResourceRequest::path_read("其他")
            .with_paths(vec![PathScope::read("其他"), PathScope::write("角色/a")]);
        assert!(table.try_acquire(&request, &holder("multi")).is_err());
        assert!(table.try_acquire(&read("其他"), &holder("probe")).is_ok());
    }

    #[test]
    fn waiting_writer_blocks_new_readers() {
        let table = Arc::new(ResourceTable::new());
        let held = table.try_acquire(&read("角色"), &holder("reader")).unwrap();

        let writer_table = table.clone();
        let writer = std::thread::spawn(move || {
            writer_table.acquire(&write("角色"), &holder("writer"), None, &|| false)
        });
        std::thread::sleep(Duration::from_millis(80));
        assert!(table.has_waiters());
        // 写者在等：新读者不能再插队。
        assert!(
            table
                .try_acquire(&read("角色"), &holder("new-reader"))
                .is_err()
        );
        drop(held);

        let guard = writer.join().unwrap().unwrap();
        drop(guard);
        assert!(table.try_acquire(&read("角色"), &holder("after")).is_ok());
        assert!(table.is_idle());
    }

    #[test]
    fn cancelled_and_timed_out_waiters_leave_no_trace() {
        let table = ResourceTable::new();
        let held = table.try_acquire(&read("a"), &holder("reader")).unwrap();

        let cancelled = table.acquire(&write("a"), &holder("writer"), None, &|| true);
        assert!(matches!(cancelled, Err(WaitError::Cancelled)));

        let timed_out = table.acquire(
            &write("a"),
            &holder("writer"),
            Some(Instant::now() + Duration::from_millis(30)),
            &|| false,
        );
        match timed_out {
            Err(WaitError::Timeout(conflicts)) => {
                assert_eq!(conflicts[0].holder.id, "reader");
                assert!(conflicts[0].sentence().contains("正在读取"));
            }
            _ => panic!("应当超时"),
        }
        drop(held);
        assert!(table.is_idle());
    }

    // 排队者还没写过一个字节：短写（评分/标签/集合）不能被它挡住。
    #[test]
    fn a_queued_task_does_not_block_short_writes() {
        use std::sync::Arc as Shared;
        let table = Shared::new(ResourceTable::new());
        // 正在跑的 AI 任务：只占 AI 服务与内容写，不写元数据。
        let running = table
            .try_acquire(
                &ResourceRequest::session_read()
                    .with_ai_service()
                    .with_content_writer(),
                &holder("embedding"),
            )
            .unwrap();

        // 描述任务排在它后面，请求里带 metadata_writer。
        let queued_table = table.clone();
        let queued = std::thread::spawn(move || {
            queued_table.acquire(
                &ResourceRequest::session_read()
                    .with_ai_service()
                    .with_content_writer()
                    .with_metadata_writer(),
                &holder("description"),
                None,
                &|| false,
            )
        });
        for _ in 0..200 {
            if table.has_waiters() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(table.has_waiters(), "描述任务应当排上队");

        let short = table.try_acquire(
            &ResourceRequest::session_read().with_metadata_writer(),
            &holder("score"),
        );
        assert!(short.is_ok(), "排队中的任务不该挡住改评分");
        drop(short);

        // 真正冲突的短写仍然按占用者拒绝。
        let blocked = table.try_acquire(
            &ResourceRequest::session_read().with_content_writer(),
            &holder("edit"),
        );
        assert!(blocked.is_err());

        drop(running);
        let guard = queued.join().unwrap().unwrap();
        drop(guard);
        assert!(table.is_idle());
    }

    /// 路径锁的写者优先保留：排队中的文件夹写者照样挡住新的读者。
    #[test]
    fn a_queued_path_writer_still_blocks_readers_and_says_so() {
        use std::sync::Arc as Shared;
        let table = Shared::new(ResourceTable::new());
        let held = table.try_acquire(&read("角色"), &holder("reader")).unwrap();

        let writer_table = table.clone();
        let writer = std::thread::spawn(move || {
            writer_table.acquire(&write("角色"), &holder("move"), None, &|| false)
        });
        for _ in 0..200 {
            if table.has_waiters() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let conflicts = table
            .try_acquire(&read("角色"), &holder("viewer"))
            .err()
            .expect("写者在等时新读者不能插队");
        assert!(conflicts[0].queued);
        assert!(
            conflicts[0].sentence().contains("排队"),
            "排队者不能说成正在进行：{}",
            conflicts[0].sentence()
        );

        drop(held);
        let guard = writer.join().unwrap().unwrap();
        drop(guard);
    }

    #[test]
    fn same_holder_can_add_short_locks() {
        let table = ResourceTable::new();
        let root = table
            .try_acquire(
                &ResourceRequest::session_read()
                    .with_content_writer()
                    .with_paths(vec![PathScope::write("")]),
                &holder("task"),
            )
            .unwrap();
        let _short = table.try_acquire(&read("角色/a"), &holder("task")).unwrap();
        assert!(
            table
                .try_acquire(&read("角色/a"), &holder("other"))
                .is_err()
        );
        drop(root);
    }

    #[test]
    fn concurrent_stress_leaves_table_idle() {
        let table = Arc::new(ResourceTable::new());
        let barrier = Arc::new(Barrier::new(9));
        let mut threads = Vec::new();
        for index in 0..8 {
            let table = table.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for step in 0..12_500 {
                    let path = format!("目录{}/子{}", (index + step) % 4, step % 3);
                    let request = if step % 3 == 0 {
                        write(&path)
                    } else {
                        read(&path)
                    };
                    let guard = table
                        .acquire(
                            &request,
                            &holder(&format!("t{index}")),
                            Some(Instant::now() + Duration::from_secs(2)),
                            &|| false,
                        )
                        .unwrap_or_else(|_| panic!("等待超时"));
                    std::thread::yield_now();
                    drop(guard);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(table.is_idle());
    }
}
