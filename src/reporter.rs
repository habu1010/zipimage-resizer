//! 進捗表示とエラー報告

use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// 進捗表示とエラー報告をまとめて扱う
///
/// 各段のスレッドへクローンして渡し、エラー件数を全段で共有する。
#[derive(Clone)]
pub struct Reporter {
    mp: MultiProgress,
    error_count: Arc<AtomicUsize>,
}

impl Reporter {
    pub fn new() -> Self {
        // テストでは進捗表示を描画しない（cargo test の出力の途中にバーが描かれるため）
        let mp = if cfg!(test) {
            MultiProgress::with_draw_target(ProgressDrawTarget::hidden())
        } else {
            MultiProgress::new()
        };
        Self {
            mp,
            error_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// パイプラインの段ごとの進捗表示を追加する（`label` は表示の先頭に付く段の名前）
    ///
    /// 件数がまだ分からない段は `len` を `None` にする（0 にすると indicatif はバーを満杯で
    /// 描くので、始まる前から完了に見えてしまう）。
    pub fn add_stage_bar(&self, label: &str, len: Option<u64>) -> ProgressBar {
        let style = ProgressStyle::with_template(
            "{prefix} [{bar:20.cyan/blue}] {pos}/{len} ETA:{eta} {msg}",
        )
        .unwrap()
        .progress_chars("#>-");
        let pb = len
            .map_or_else(ProgressBar::no_length, ProgressBar::new)
            .with_style(style)
            .with_prefix(label.to_string());
        self.mp.add(pb)
    }

    /// エラーを stderr に出力して件数を数える
    ///
    /// プログレスバーの描画を一時停止しないと表示が崩れるため、`suspend` 経由で出力する。
    pub fn report_error(&self, msg: String) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.mp.suspend(|| eprintln!("{msg}"));
    }

    /// `path` についての処理（`action`）の失敗を、原因まで含めて報告する
    pub fn report_failure(&self, action: &str, path: &Path, e: impl std::fmt::Display) {
        self.report_error(format!("Error {action} {}: {e:#}", path.display()));
    }

    pub fn error_count(&self) -> usize {
        self.error_count.load(Ordering::Relaxed)
    }
}
