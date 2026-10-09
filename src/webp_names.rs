//! 変換後の WebP の出力名を決める
//!
//! 単純に拡張子を `.webp` に変えると、同名の画像同士や既存の WebP と衝突して画像が失われるため、
//! 衝突しない名前を割り当てる。

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::fs_util::{display_in, parent_dir};

/// 衝突判定用のキー（大文字小文字を区別しないファイルシステムも考慮する）
fn name_key(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

/// 衝突がない場合の変換後のファイル名（`001.png` → `001.webp`）
fn natural_webp_name(src: &Path) -> OsString {
    src.with_extension("webp")
        .file_name()
        .unwrap()
        .to_os_string()
}

/// 衝突する場合の変換後のファイル名の候補を優先順に返す
///
/// `001.png` → `001.png.webp`, `001.png.1.webp`, `001.png.2.webp`, …
/// 拡張子のない `001` → `001.1.webp`, `001.2.webp`, …
fn fallback_webp_names(src: &Path) -> impl Iterator<Item = OsString> {
    let name = src.file_name().unwrap().to_os_string();
    // 拡張子のない `001` では `001.webp` が衝突しないときの名前と同じになり、衝突を避けたことに
    // ならないので、連番から始める（`001.1.webp`, `001.2.webp`, …）
    let start = if src.extension().is_none() { 1 } else { 0 };

    (start..).map(move |n| {
        let mut candidate = name.clone();
        if n > 0 {
            candidate.push(format!(".{n}"));
        }
        candidate.push(".webp");
        candidate
    })
}

/// 一時ファイルに書き出した変換結果を、既存のファイルと衝突しない最終的な名前へ rename する
///
/// 単純に拡張子を `.webp` に変えると、`001.jpg` と `001.png` が同じ `001.webp` に
/// なったり、変換対象外の既存の `001.webp` を上書きしたりして画像が失われる。
/// 衝突する場合は元の拡張子を残した名前（`001.png.webp`）にする。
///
/// 変換されたファイル同士が衝突する場合は、全員を元の拡張子を残した名前にする。
/// 片方だけを `001.webp` にすると、`001.png.webp` < `001.webp` の順に並んで
/// ページ順が逆転するため。拡張子のない `001` と `001.png` の場合は、`001.1.webp` と
/// `001.png.webp` にする。
///
/// 実際に変換されたファイルだけを対象に、ディレクトリ内に実在する名前
/// （ディレクトリも含む）と照合して決める。パス順に割り当てるので、
/// 結果は並列処理の実行順に左右されない。
///
/// エラーには、一時ディレクトリの絶対パスではなく `base_dir` からの相対パス（zip 内のパス）を示す。
pub fn rename_to_final_names(
    mut converted: Vec<(PathBuf, PathBuf)>,
    base_dir: &Path,
) -> Result<()> {
    converted.sort();

    // 変換後の名前同士の衝突を数える
    let mut natural_counts: HashMap<(&Path, String), usize> = HashMap::new();
    for (src, _) in &converted {
        let key = (parent_dir(src), name_key(&natural_webp_name(src)));
        *natural_counts.entry(key).or_default() += 1;
    }

    let pending: HashSet<&Path> = converted.iter().map(|(_, tmp)| tmp.as_path()).collect();
    // ディレクトリごとの使用済みの名前
    let mut taken: HashMap<&Path, HashSet<String>> = HashMap::new();

    for (src, tmp) in &converted {
        let dir = parent_dir(src);
        let names = match taken.entry(dir) {
            Entry::Occupied(e) => e.into_mut(),
            // 未 rename の一時ファイルは使用済みとして扱わない
            Entry::Vacant(e) => e.insert(existing_names(dir, &pending).with_context(|| {
                format!("failed to read directory {}", display_in(dir, base_dir))
            })?),
        };

        let natural = natural_webp_name(src);
        let key = (dir, name_key(&natural));
        let name = if natural_counts[&key] == 1 && !names.contains(&key.1) {
            natural
        } else {
            fallback_webp_names(src)
                .find(|candidate| !names.contains(&name_key(candidate)))
                .unwrap()
        };
        names.insert(name_key(&name));
        let dst = dir.join(name);
        std::fs::rename(tmp, &dst).with_context(|| {
            format!(
                "failed to rename the converted image to {}",
                display_in(&dst, base_dir)
            )
        })?;
    }

    Ok(())
}

/// `dir` 直下にある名前（ディレクトリも含む）の衝突判定用キーを集める（`exclude` は除く）
fn existing_names(dir: &Path, exclude: &HashSet<&Path>) -> std::io::Result<HashSet<String>> {
    let mut names = HashSet::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if !exclude.contains(path.as_path()) {
            names.insert(name_key(path.file_name().unwrap()));
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::test_util::list_names;

    #[test]
    fn test_fallback_webp_names() {
        let candidates: Vec<_> = super::fallback_webp_names(Path::new("/d/001.png"))
            .take(3)
            .collect();
        assert_eq!(
            candidates,
            ["001.png.webp", "001.png.1.webp", "001.png.2.webp"]
        );

        // 拡張子がない場合は、衝突しないときの名前（001.webp）を候補に含めない
        let candidates: Vec<_> = super::fallback_webp_names(Path::new("/d/001"))
            .take(2)
            .collect();
        assert_eq!(candidates, ["001.1.webp", "001.2.webp"]);
    }

    #[test]
    fn test_rename_to_final_names_numbering() {
        // 元の拡張子を残した名前まで使用済みなら連番を付ける
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("001.png.webp"), b"existing").unwrap();
        let tmp_jpg = dir.path().join("a.webp.tmp");
        let tmp_png = dir.path().join("b.webp.tmp");
        std::fs::write(&tmp_jpg, b"jpg").unwrap();
        std::fs::write(&tmp_png, b"png").unwrap();

        super::rename_to_final_names(
            vec![
                (dir.path().join("001.png"), tmp_png),
                (dir.path().join("001.jpg"), tmp_jpg),
            ],
            dir.path(),
        )
        .unwrap();

        assert_eq!(
            list_names(dir.path()),
            ["001.jpg.webp", "001.png.1.webp", "001.png.webp"]
        );
        assert_eq!(
            std::fs::read(dir.path().join("001.png.1.webp")).unwrap(),
            b"png"
        );
        assert_eq!(
            std::fs::read(dir.path().join("001.png.webp")).unwrap(),
            b"existing"
        );
    }

    #[test]
    fn test_rename_to_final_names_without_extension() {
        // 拡張子のない画像と衝突しても、ページ順（001 → 001.png）が逆転しない名前にする
        let dir = tempfile::tempdir().unwrap();
        let tmp_noext = dir.path().join("a.webp.tmp");
        let tmp_png = dir.path().join("b.webp.tmp");
        std::fs::write(&tmp_noext, b"noext").unwrap();
        std::fs::write(&tmp_png, b"png").unwrap();

        super::rename_to_final_names(
            vec![
                (dir.path().join("001"), tmp_noext),
                (dir.path().join("001.png"), tmp_png),
            ],
            dir.path(),
        )
        .unwrap();

        assert_eq!(list_names(dir.path()), ["001.1.webp", "001.png.webp"]);
        assert_eq!(
            std::fs::read(dir.path().join("001.1.webp")).unwrap(),
            b"noext"
        );
    }
}
