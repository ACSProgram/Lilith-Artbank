//! 终态与摘要的单元测试。

use crate::{
    app::tasks::TaskPhase,
    library::pack_gc::{CompactReport, CompactStop},
};

use super::maintenance::{optimize_outcome, pack_compact_outcome};

fn compact_report(stop: CompactStop, segments: u64, delete_failed: u64) -> CompactReport {
    CompactReport {
        segments,
        freed_bytes: segments * 1024 * 1024,
        delete_failed,
        stop,
        error: (stop == CompactStop::Failed).then(|| "缩略图指针越界".to_owned()),
    }
}

#[test]
fn compact_without_problems_is_completed() {
    let (phase, message) = pack_compact_outcome(&compact_report(CompactStop::Done, 2, 0));
    assert_eq!(phase, TaskPhase::Completed);
    assert!(message.contains("已回收 2 个段"), "{message}");
    assert!(!message.contains("删除失败"), "{message}");
}

/// 旧段删不掉时释放字节不算数，结果只能是「部分失败」。
#[test]
fn compact_with_undeleted_segments_is_partial() {
    let (phase, message) = pack_compact_outcome(&compact_report(CompactStop::Done, 2, 1));
    assert_eq!(phase, TaskPhase::PartialFailed);
    assert!(message.contains("1 个旧段删除失败"), "{message}");
}

/// 半途出错也要交出已回收的部分，而不是整份失败。
#[test]
fn compact_failing_after_a_segment_is_partial() {
    let (phase, message) = pack_compact_outcome(&compact_report(CompactStop::Failed, 1, 0));
    assert_eq!(phase, TaskPhase::PartialFailed);
    assert!(message.contains("缩略图指针越界"), "{message}");
}

/// 一段都没回收就出错，没有什么可以保留的，直接失败。
#[test]
fn compact_failing_before_any_segment_is_failed() {
    let (phase, _) = pack_compact_outcome(&compact_report(CompactStop::Failed, 0, 0));
    assert_eq!(phase, TaskPhase::Failed);
}

#[test]
fn compact_yielded_and_cancelled_keep_their_phases() {
    assert_eq!(
        pack_compact_outcome(&compact_report(CompactStop::Yielded, 1, 0)).0,
        TaskPhase::Yielded
    );
    assert_eq!(
        pack_compact_outcome(&compact_report(CompactStop::Cancelled, 1, 0)).0,
        TaskPhase::Cancelled
    );
}

fn optimize_report() -> crate::library::optimize::OptimizeReport {
    crate::library::optimize::OptimizeReport {
        processed: 10,
        savings_bytes: 1024 * 1024,
        ..Default::default()
    }
}

/// 旧原图删不掉时“省 X”不成立：终态是「部分失败」，摘要要点名未删张数。
#[test]
fn optimize_with_undeleted_originals_is_partial() {
    let mut report = optimize_report();
    report.delete_failed = 3;
    report.deleted_files = 7;
    let (phase, summary) = optimize_outcome(&report, 0, false);
    assert_eq!(phase, TaskPhase::PartialFailed);
    assert!(summary.contains("3 张旧原图未删除"), "{summary}");
}

#[test]
fn optimize_without_problems_is_completed() {
    let (phase, summary) = optimize_outcome(&optimize_report(), 2, false);
    assert_eq!(phase, TaskPhase::Completed);
    assert!(summary.contains("已替换 10 张"), "{summary}");
    assert!(!summary.contains("未删除"), "{summary}");
}

/// 连续失败提前停止仍是彻底失败，不因为删掉了部分旧图就变「部分失败」。
#[test]
fn optimize_stopped_early_is_failed() {
    let mut report = optimize_report();
    report.delete_failed = 1;
    report.stop_reason = Some("连续失败过多".to_owned());
    let (phase, summary) = optimize_outcome(&report, 0, false);
    assert_eq!(phase, TaskPhase::Failed);
    assert!(summary.contains("1 张旧原图未删除"), "{summary}");
}

/// 取消是中途停止：跑了多少张都不能写成「完成」或「部分失败」。
#[test]
fn optimize_cancelled_is_cancelled() {
    let mut report = optimize_report();
    report.cancelled = true;
    let (phase, _) = optimize_outcome(&report, 0, true);
    assert_eq!(phase, TaskPhase::Cancelled);
}
