//! 各入力の形式と出力先を決め、処理対象（`ArchiveJob`）を計画する

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    fmt,
    path::{Path, PathBuf},
};

use crate::archive::ArchiveFormat;
use crate::fs_util::parent_dir;

/// `plan_jobs` の結果
#[derive(Debug)]
pub struct Plan {
    /// 処理するアーカイブ（指定された順）
    pub jobs: Vec<ArchiveJob>,
    /// 処理しない入力とその理由（判定した順。読めない・ファイルでない入力を先に、
    /// 出力先の検査で除いた入力を後に、それぞれ指定された順で並べる）
    pub skipped: Vec<Skipped>,
}

/// 処理しない入力
#[derive(Debug)]
pub struct Skipped {
    /// 指定されたままの入力のパス
    pub src: PathBuf,
    pub reason: SkipReason,
}

/// 入力を処理しない理由
#[derive(Debug)]
pub enum SkipReason {
    /// 入力を開けない・読めない（存在しないなど）
    Unreadable(std::io::Error),
    /// 入力がファイルでない（ディレクトリなど）
    NotAFile,
    /// 出力先 `dst` のパスを正規化できない（親ディレクトリが無いなど）
    UnresolvableOutput { dst: PathBuf, error: std::io::Error },
    /// 出力先 `dst` が入力ファイル（自身でも他の入力でも）と重なる
    OverwritesInput { dst: PathBuf },
    /// `-d` に入力元と同じディレクトリを指定し、拡張子を変えた出力先 `dst` が既に存在する
    OutputExistsInInputDir { dst: PathBuf },
    /// 出力先 `dst` が、先に指定された入力 `other` の出力先と重なる
    SameOutputAs { dst: PathBuf, other: PathBuf },
}

/// ユーザーに表示する報告の文面
impl fmt::Display for Skipped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let src = self.src.display();
        match &self.reason {
            // 下の UnresolvableOutput と共に、Reporter::report_failure と同じ書式
            SkipReason::Unreadable(e) => write!(f, "Error reading {src}: {e:#}"),
            SkipReason::NotAFile => write!(f, "Skipping {src}: not a file"),
            SkipReason::UnresolvableOutput { dst, error } => {
                write!(f, "Error resolving {}: {error:#}", dst.display())
            }
            SkipReason::OverwritesInput { dst } => write!(
                f,
                "Skipping {src}: output {} would overwrite an input file",
                dst.display()
            ),
            SkipReason::OutputExistsInInputDir { dst } => write!(
                f,
                "Skipping {src}: output {} already exists in the input directory",
                dst.display()
            ),
            SkipReason::SameOutputAs { dst, other } => write!(
                f,
                "Skipping {src}: output {} is also the output of {}",
                dst.display(),
                other.display()
            ),
        }
    }
}

/// 処理するアーカイブ（zip / RAR）とその出力先（zip）の組
///
/// 形式と出力先は `plan_jobs` で検証済みのものを一度だけ決め、後段はそれをそのまま使う。
#[derive(Debug, PartialEq)]
pub struct ArchiveJob {
    pub src_file: PathBuf,
    pub format: ArchiveFormat,
    pub dst_file: PathBuf,
}

fn append_suffix_to_filename<P: AsRef<Path>>(path: P, suffix: &str) -> PathBuf {
    let path = path.as_ref();
    let mut stem = path.file_stem().unwrap().to_os_string();
    let ext = path.extension().unwrap_or(std::ffi::OsStr::new(""));

    stem.push(suffix);
    path.with_file_name(stem).with_extension(ext)
}

/// 出力先のパスを決める
///
/// 出力は常に zip なので、RAR 用の拡張子（`.rar` / `.cbr`）は中身によらず `.zip` にする。
/// zip 用の拡張子（`.zip` / `.cbz`）は、中身が zip ならそのまま残し、RAR なら `.zip` にする。
/// RAR の入力のそれ以外の拡張子（`Vol.1` の `.1` など）は名前の一部かもしれないので、
/// 残して `.zip` を足す（zip の入力は従来どおりそのまま）。
fn determine_output_path(path: &Path, format: ArchiveFormat, output_dir: Option<&Path>) -> PathBuf {
    let dst = match output_dir {
        Some(output_dir) => output_dir.join(path.file_name().unwrap()),
        None => append_suffix_to_filename(path, "_resized"),
    };
    let has_extension = |exts: &[&str]| {
        dst.extension()
            .is_some_and(|ext| exts.iter().any(|e| ext.eq_ignore_ascii_case(e)))
    };
    let is_rar = format == ArchiveFormat::Rar;
    if has_extension(&["rar", "cbr"]) || (is_rar && has_extension(&["zip", "cbz"])) {
        dst.with_extension("zip")
    } else if is_rar {
        let mut dst = dst.into_os_string();
        dst.push(".zip");
        dst.into()
    } else {
        dst
    }
}

/// `-d` に入力元と同じディレクトリを指定したときに、拡張子を変えた出力先（`a.rar` → `a.zip`）が
/// 既に存在するかどうか
///
/// 入力や既存の出力がシンボリックリンクの場合も考え、リンク自体の置き場所で比べる
/// （入力はリンク先のディレクトリも入力元とみなす）。
fn output_exists_in_input_dir(src: &Path, canonical_src: &Path, dst: &Path) -> bool {
    let Ok(dst_dir) = parent_dir(dst).canonicalize() else {
        return false;
    };
    let in_input_dir = Some(dst_dir.as_path()) == canonical_src.parent()
        || parent_dir(src)
            .canonicalize()
            .is_ok_and(|src_dir| src_dir == dst_dir);
    in_input_dir && dst.symlink_metadata().is_ok()
}

/// 同じファイルかどうかを比較できるよう、パスを正規化する
///
/// 存在しないパス（これから作る出力先など）は、親ディレクトリだけを正規化する。
fn normalize_path(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().or_else(|_| {
        let parent = parent_dir(path).canonicalize()?;
        Ok(parent.join(path.file_name().unwrap_or_default()))
    })
}

/// 各入力の出力先を決め、原本や他の出力を上書きするものを除いた処理対象を返す
///
/// 例えば `-d` に入力元と同じディレクトリを指定すると出力先が入力ファイル自身になり、
/// 原本が壊れる。展開・変換を始める前に、正規化したパスで以下を検証する。
///
/// - 出力先が入力ファイル（自身でも他の入力でも）と重なる入力は処理しない
/// - 出力先が重なる場合は、先に指定された入力だけを処理する
/// - 同じファイルが重複して指定された場合は、1 つにまとめる
/// - `-d` に入力元と同じディレクトリを指定し、拡張子を変えた出力先（`a.rar` → `a.zip`）が
///   既に存在する場合は処理しない（入力に含まれていなくても、同じ本の zip 版の原本かもしれないため）
///
/// 処理しない入力は、理由とともに `Plan::skipped` で返す（報告は呼び出し側で行う）。
///
/// # Errors
/// 出力先ディレクトリが存在しない・ディレクトリでない場合は、全体をエラーにする
/// （展開・変換を始める前に失敗させ、最終段で全入力が失敗するのを防ぐ。作成はしない）。
pub fn plan_jobs(archives: Vec<PathBuf>, output_dir: Option<&Path>) -> anyhow::Result<Plan> {
    if let Some(output_dir) = output_dir {
        if !output_dir.exists() {
            anyhow::bail!("output directory not found: {}", output_dir.display());
        }
        if !output_dir.is_dir() {
            anyhow::bail!("output path is not a directory: {}", output_dir.display());
        }
    }

    let mut skipped = Vec::new();
    let mut skip = |src, reason| skipped.push(Skipped { src, reason });

    let mut inputs = Vec::new();
    let mut input_paths = HashSet::new();
    for src in archives {
        let canonical = match src.canonicalize() {
            Ok(canonical) => canonical,
            Err(e) => {
                skip(src, SkipReason::Unreadable(e));
                continue;
            }
        };
        if !canonical.is_file() {
            skip(src, SkipReason::NotAFile);
            continue;
        }
        if !input_paths.insert(canonical.clone()) {
            continue;
        }
        match ArchiveFormat::detect(&canonical) {
            Ok(format) => inputs.push((src, canonical, format)),
            Err(e) => skip(src, SkipReason::Unreadable(e)),
        }
    }

    let mut outputs: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut jobs = Vec::new();
    for (src, canonical, format) in inputs {
        let dst = determine_output_path(&src, format, output_dir);
        let normalized = match normalize_path(&dst) {
            Ok(normalized) => normalized,
            Err(error) => {
                skip(src, SkipReason::UnresolvableOutput { dst, error });
                continue;
            }
        };

        if input_paths.contains(&normalized) {
            skip(src, SkipReason::OverwritesInput { dst });
            continue;
        }
        // 出力先が入力自身の場合は上で除いているので、ここで見つかるのは拡張子を .zip にした
        // 出力先（入力がシンボリックリンクなら、リンク先と名前が違う場合も含む）
        if output_dir.is_some() && output_exists_in_input_dir(&src, &canonical, &dst) {
            skip(src, SkipReason::OutputExistsInInputDir { dst });
            continue;
        }
        match outputs.entry(normalized) {
            Entry::Occupied(entry) => {
                let other = entry.get().clone();
                skip(src, SkipReason::SameOutputAs { dst, other });
            }
            Entry::Vacant(entry) => {
                entry.insert(src.clone());
                jobs.push(ArchiveJob {
                    src_file: src,
                    format,
                    dst_file: dst,
                });
            }
        }
    }

    Ok(Plan { jobs, skipped })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{SkipReason, Skipped};
    use crate::test_util::{copy_rar_fixture, touch_files};

    #[test]
    fn test_append_suffix_to_filename() {
        // サフィックスはそのまま付与される（区切り文字は呼び出し側が含める）
        let path = super::append_suffix_to_filename(Path::new("/path1/path2/test.txt"), "_suffix");
        assert_eq!(path, Path::new("/path1/path2/test_suffix.txt"));
    }

    #[test]
    fn test_append_suffix_to_filename_without_extension() {
        let path = super::append_suffix_to_filename(Path::new("/path1/path2/test"), "_suffix");
        assert_eq!(path, Path::new("/path1/path2/test_suffix"));
    }

    #[test]
    fn test_determine_output_path() {
        use super::ArchiveFormat::{Rar, Zip};

        let src = Path::new("/path1/path2/test.cbz");

        // 出力先ディレクトリ指定時はファイル名だけを引き継ぐ
        let path = super::determine_output_path(src, Zip, Some(Path::new("/out")));
        assert_eq!(path, Path::new("/out/test.cbz"));

        // 未指定時は元ファイル名に _resized を付ける
        let path = super::determine_output_path(src, Zip, None);
        assert_eq!(path, Path::new("/path1/path2/test_resized.cbz"));

        // RAR の出力は zip なので、拡張子を .zip にする
        for src in [
            "/path1/path2/test.rar",
            "/path1/path2/test.cbr",
            "/path1/path2/test.RAR",
        ] {
            let src = Path::new(src);
            let path = super::determine_output_path(src, Rar, Some(Path::new("/out")));
            assert_eq!(path, Path::new("/out/test.zip"));
            let path = super::determine_output_path(src, Rar, None);
            assert_eq!(path, Path::new("/path1/path2/test_resized.zip"));
        }

        // .rar / .cbr 以外の拡張子は名前の一部として残す（Vol.1 と Vol.2 が同じ出力にならない）
        let src = Path::new("/path1/path2/Vol.1");
        let path = super::determine_output_path(src, Rar, Some(Path::new("/out")));
        assert_eq!(path, Path::new("/out/Vol.1.zip"));
        let path = super::determine_output_path(src, Rar, None);
        assert_eq!(path, Path::new("/path1/path2/Vol_resized.1.zip"));

        // 中身が RAR で拡張子が .zip / .cbz の場合も、.zip を重ねない
        let path = super::determine_output_path(Path::new("/p/test.cbz"), Rar, None);
        assert_eq!(path, Path::new("/p/test_resized.zip"));

        // 中身が zip で拡張子が .cbr の場合も、出力は zip なので拡張子を .zip にする
        let path = super::determine_output_path(Path::new("/p/test.cbr"), Zip, None);
        assert_eq!(path, Path::new("/p/test_resized.zip"));
    }

    /// `plan_jobs` を実行し、処理対象の入力と、処理しない入力を返す
    fn plan(zipfiles: Vec<PathBuf>, output_dir: Option<PathBuf>) -> (Vec<PathBuf>, Vec<Skipped>) {
        let planned = super::plan_jobs(zipfiles, output_dir.as_deref()).unwrap();
        let srcs = planned.jobs.into_iter().map(|job| job.src_file).collect();
        (srcs, planned.skipped)
    }

    /// 処理しない入力がちょうど 1 つであることを確かめ、その理由を返す
    fn only_reason(skipped: &[Skipped]) -> &SkipReason {
        match skipped {
            [skipped] => &skipped.reason,
            _ => panic!("expected exactly one skipped input: {skipped:?}"),
        }
    }

    #[test]
    fn test_plan_jobs_ok() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip", "b.zip"]);

        let planned = super::plan_jobs(zipfiles.clone(), Some(&out)).unwrap();
        assert!(planned.skipped.is_empty(), "{:?}", planned.skipped);
        assert_eq!(
            planned.jobs,
            [
                super::ArchiveJob {
                    src_file: zipfiles[0].clone(),
                    format: super::ArchiveFormat::Zip,
                    dst_file: out.join("a.zip"),
                },
                super::ArchiveJob {
                    src_file: zipfiles[1].clone(),
                    format: super::ArchiveFormat::Zip,
                    dst_file: out.join("b.zip"),
                },
            ]
        );
    }

    #[test]
    fn test_plan_jobs_output_dir_is_input_dir() {
        // -d に入力元と同じディレクトリを指定すると、原本を上書きしてしまう
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip"]);

        let (srcs, skipped) = plan(zipfiles, Some(dir.path().join(".")));
        assert!(srcs.is_empty());
        assert!(matches!(
            only_reason(&skipped),
            SkipReason::OverwritesInput { .. }
        ));
    }

    #[test]
    fn test_plan_jobs_output_is_another_input() {
        // 再実行時に前回の出力が glob に含まれた場合など。a.zip の出力先 a_resized.zip が
        // 別の入力と重なるので a.zip だけを除き、残りは処理を続ける
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip", "a_resized.zip", "b.zip"]);

        let (srcs, skipped) = plan(zipfiles.clone(), None);
        assert_eq!(srcs, &zipfiles[1..]);
        assert!(matches!(
            only_reason(&skipped),
            SkipReason::OverwritesInput { .. }
        ));
    }

    #[test]
    fn test_plan_jobs_same_output() {
        // -d 指定時、別ディレクトリの同名 zip は同じ出力先になるので、先に指定した方だけを処理する
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        let zipfiles = touch_files(dir.path(), &["x/a.zip", "y/a.zip"]);

        let (srcs, skipped) = plan(zipfiles.clone(), Some(out));
        assert_eq!(srcs, &zipfiles[..1]);
        assert!(
            matches!(&skipped[..], [Skipped { src, reason: SkipReason::SameOutputAs { other, .. } }] if *src == zipfiles[1] && *other == zipfiles[0]),
            "{skipped:?}"
        );
    }

    #[test]
    fn test_plan_jobs_rar() {
        // 形式は中身で判定し、RAR の出力先は拡張子を .zip にする
        let dir = tempfile::tempdir().unwrap();
        let src = copy_rar_fixture(dir.path(), "a.cbr");

        let planned = super::plan_jobs(vec![src.clone()], None).unwrap();
        assert!(planned.skipped.is_empty(), "{:?}", planned.skipped);
        assert_eq!(
            planned.jobs,
            [super::ArchiveJob {
                src_file: src,
                format: super::ArchiveFormat::Rar,
                dst_file: dir.path().join("a_resized.zip"),
            }]
        );
    }

    #[test]
    fn test_plan_jobs_rar_output_exists_in_input_dir() {
        // -d に入力元と同じディレクトリを指定すると、a.rar の出力先は隣の a.zip になる。
        // 入力に含まれていなくても同じ本の zip 版の原本かもしれないので、上書きしない
        let dir = tempfile::tempdir().unwrap();
        let src = copy_rar_fixture(dir.path(), "a.rar");
        let other = copy_rar_fixture(dir.path(), "b.rar");
        touch_files(dir.path(), &["a.zip"]);

        let (srcs, skipped) = plan(
            vec![src.clone(), other.clone()],
            Some(dir.path().to_path_buf()),
        );
        assert_eq!(srcs, [other]);
        assert!(matches!(
            only_reason(&skipped),
            SkipReason::OutputExistsInInputDir { .. }
        ));

        // 中身が zip の c.cbr も出力先が c.zip になるので、同様に上書きしない
        let cbr = touch_files(dir.path(), &["c.cbr", "c.zip"]).remove(0);
        let (srcs, skipped) = plan(vec![cbr], Some(dir.path().to_path_buf()));
        assert!(srcs.is_empty());
        assert!(matches!(
            only_reason(&skipped),
            SkipReason::OutputExistsInInputDir { .. }
        ));

        // 別のディレクトリへの出力では、既存の出力は従来どおり上書きする
        let out = dir.path().join("out");
        touch_files(&out, &["a.zip"]);
        let (srcs, skipped) = plan(vec![src], Some(out));
        assert_eq!(srcs.len(), 1);
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    #[test]
    #[cfg(unix)]
    fn test_plan_jobs_rar_output_exists_next_to_symlink() {
        // 入力がシンボリックリンクでも、リンクのあるディレクトリの a.zip を上書きしない
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let target = copy_rar_fixture(&real, "a.rar");
        let books = dir.path().join("books");
        touch_files(&books, &["a.zip"]);
        let link = books.join("a.rar");
        std::os::unix::fs::symlink(target, &link).unwrap();

        let (srcs, skipped) = plan(vec![link], Some(books));
        assert!(srcs.is_empty());
        assert!(matches!(
            only_reason(&skipped),
            SkipReason::OutputExistsInInputDir { .. }
        ));
    }

    #[test]
    #[cfg(unix)]
    fn test_plan_jobs_rar_symlink_named_zip() {
        // 中身が RAR の real/a.rar を books/a.zip というリンクで渡し、-d にリンク先の
        // ディレクトリを指定しても、real/a.zip を上書きしない
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let target = copy_rar_fixture(&real, "a.rar");
        touch_files(&real, &["a.zip"]);
        let books = dir.path().join("books");
        std::fs::create_dir(&books).unwrap();
        let link = books.join("a.zip");
        std::os::unix::fs::symlink(target, &link).unwrap();

        let (srcs, skipped) = plan(vec![link], Some(real));
        assert!(srcs.is_empty());
        assert!(matches!(
            only_reason(&skipped),
            SkipReason::OutputExistsInInputDir { .. }
        ));
    }

    #[test]
    #[cfg(unix)]
    fn test_plan_jobs_rar_existing_output_is_symlink() {
        // 入力元にある a.zip が別の場所へのシンボリックリンクでも、上書きしない
        let dir = tempfile::tempdir().unwrap();
        let target = touch_files(dir.path(), &["library/a.zip"]).remove(0);
        let books = dir.path().join("books");
        std::fs::create_dir(&books).unwrap();
        let src = copy_rar_fixture(&books, "a.rar");
        std::os::unix::fs::symlink(target, books.join("a.zip")).unwrap();

        let (srcs, skipped) = plan(vec![src], Some(books));
        assert!(srcs.is_empty());
        assert!(matches!(
            only_reason(&skipped),
            SkipReason::OutputExistsInInputDir { .. }
        ));
    }

    #[test]
    fn test_plan_jobs_zip_and_rar_same_output() {
        // a.zip と a.rar はどちらも出力先が a_resized.zip になるので、先に指定した方だけを処理する
        let dir = tempfile::tempdir().unwrap();
        let zip = touch_files(dir.path(), &["a.zip"]).remove(0);
        let rar = copy_rar_fixture(dir.path(), "a.rar");

        let (srcs, skipped) = plan(vec![zip.clone(), rar.clone()], None);
        assert_eq!(srcs, std::slice::from_ref(&zip));
        assert!(
            matches!(&skipped[..], [Skipped { src, reason: SkipReason::SameOutputAs { other, .. } }] if *src == rar && *other == zip),
            "{skipped:?}"
        );
    }

    #[test]
    fn test_plan_jobs_duplicate_input() {
        // 同じファイルを別の書き方で重複して指定しても、エラーにせず 1 つにまとめる
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip"]);
        let duplicate = dir.path().join(".").join("a.zip");

        let (srcs, skipped) = plan(vec![zipfiles[0].clone(), duplicate], None);
        assert_eq!(srcs, zipfiles);
        assert!(skipped.is_empty(), "{skipped:?}");
    }

    #[test]
    fn test_plan_jobs_invalid_input() {
        // 存在しない入力やディレクトリは理由とともに除く（panic しない）
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = vec![
            dir.path().join("missing.zip"),
            dir.path().join("."),
            PathBuf::from("/"),
        ];

        let (srcs, skipped) = plan(zipfiles, None);
        assert!(srcs.is_empty());
        let reasons: Vec<_> = skipped.iter().map(|skipped| &skipped.reason).collect();
        assert!(
            matches!(
                reasons[..],
                [
                    SkipReason::Unreadable(_),
                    SkipReason::NotAFile,
                    SkipReason::NotAFile
                ]
            ),
            "{skipped:?}"
        );
    }

    #[test]
    fn test_plan_jobs_invalid_output_dir() {
        // 出力先ディレクトリが存在しない・ディレクトリでない場合は、入力ごとではなく全体をエラーにする
        let dir = tempfile::tempdir().unwrap();
        let zipfiles = touch_files(dir.path(), &["a.zip"]);
        for output_dir in [dir.path().join("missing"), zipfiles[0].clone()] {
            let result = super::plan_jobs(zipfiles.clone(), Some(&output_dir));
            assert!(result.is_err(), "{}", output_dir.display());
        }
    }

    #[test]
    fn test_skipped_display() {
        // ユーザーに表示する報告の文面
        let skipped = |reason| super::Skipped {
            src: PathBuf::from("a.rar"),
            reason,
        };
        let dst = || PathBuf::from("out/a.zip");
        let io_error = || std::io::Error::other("boom");
        for (reason, expected) in [
            (
                SkipReason::Unreadable(io_error()),
                "Error reading a.rar: boom",
            ),
            (SkipReason::NotAFile, "Skipping a.rar: not a file"),
            (
                SkipReason::UnresolvableOutput {
                    dst: dst(),
                    error: io_error(),
                },
                "Error resolving out/a.zip: boom",
            ),
            (
                SkipReason::OverwritesInput { dst: dst() },
                "Skipping a.rar: output out/a.zip would overwrite an input file",
            ),
            (
                SkipReason::OutputExistsInInputDir { dst: dst() },
                "Skipping a.rar: output out/a.zip already exists in the input directory",
            ),
            (
                SkipReason::SameOutputAs {
                    dst: dst(),
                    other: PathBuf::from("a.zip"),
                },
                "Skipping a.rar: output out/a.zip is also the output of a.zip",
            ),
        ] {
            assert_eq!(skipped(reason).to_string(), expected);
        }
    }
}
