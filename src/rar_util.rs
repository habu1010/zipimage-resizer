use std::cell::Cell;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Once;

use anyhow::{bail, Context};
use indicatif::ProgressBar;
use unrar::{Archive, VolumeInfo};

use crate::fs_util::parent_dir;

/// RAR5 の先頭のシグネチャ
pub const RAR5_SIGNATURE: &[u8] = b"Rar!\x1a\x07\x01\x00";

thread_local! {
    /// このスレッドで `catch_panic` の中を実行しているか
    static CATCHING_PANIC: Cell<bool> = const { Cell::new(false) };
}

/// unrar クレートの呼び出しで起きた panic をエラーにする
///
/// unrar 0.5.8 は、知らないエラーコード（辞書サイズが大きすぎる場合の ERAR_LARGE_DICT など）を
/// UnRAR が返すと unwrap で panic する。1 ファイルのためにパイプライン全体を止めないよう、
/// エラーとして報告できる形にする。
///
/// 既定の panic フックは panic の内容を stderr に直接出し、プログレスバーの描画を崩すので、
/// ここで捕まえる panic ではフックの出力を抑える（内容は戻り値のエラーとして報告される）。
fn catch_panic<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    static INSTALL_HOOK: Once = Once::new();
    INSTALL_HOOK.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !CATCHING_PANIC.get() {
                default_hook(info);
            }
        }));
    });

    CATCHING_PANIC.set(true);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    CATCHING_PANIC.set(false);
    result.unwrap_or_else(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown panic");
        bail!("unsupported or corrupted RAR archive (RAR library error: {msg})")
    })
}

/// rarファイルに含まれるエントリ数（ディレクトリを含む）を取得する
/// # Errors
/// rarファイルが存在しない・壊れている場合と、対応していない RAR（分割 RAR、途中で切れた RAR5）の
/// 場合にエラーを返す
pub fn get_file_count<P: AsRef<Path>>(rar_file: P) -> anyhow::Result<usize> {
    catch_panic(|| validated_entry_count(rar_file.as_ref()))
}

/// 対応している RAR かを確かめてから、エントリ数を数える
///
/// 分割 RAR と途中で切れた RAR5 を弾く。展開（`extract_entries`）もこれを通るので、
/// サイズチェックを経ずに展開する場合も同じ検査がかかる。
fn validated_entry_count(rar_file: &Path) -> anyhow::Result<usize> {
    let archive = Archive::new(rar_file).open_for_listing()?;
    // 分割 RAR は対応していない
    if !matches!(archive.volume_info(), VolumeInfo::None) {
        bail!("multi-volume RAR archives are not supported");
    }

    let mut count = 0;
    for entry in archive {
        entry?;
        count += 1;
    }
    ensure_rar5_complete(rar_file)?;
    Ok(count)
}

/// 終端ヘッダを探す末尾の範囲（終端ヘッダは通常 8 バイト。後ろに少しの余分なデータがあってもよい）
const END_HEADER_SEARCH_LEN: u64 = 64;

/// RAR5 の末尾に終端ヘッダがあることを確かめる
///
/// エントリの境界で切れた RAR（ダウンロード途中など）は、UnRAR では最後まで読めた場合と
/// 区別できない（unrar クレートは終端ヘッダの有無を返さない）。ページが欠けたまま変換しない
/// よう、末尾を自前で調べる。RAR4 は終端ヘッダの無い古い形式（RAR 2.x）もあるので調べない。
fn ensure_rar5_complete(rar_file: &Path) -> anyhow::Result<()> {
    let mut file = File::open(rar_file)?;
    let mut signature = [0; RAR5_SIGNATURE.len()];
    if file.read_exact(&mut signature).is_err() || signature != RAR5_SIGNATURE {
        return Ok(());
    }

    let tail_len = file.metadata()?.len().min(END_HEADER_SEARCH_LEN);
    file.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)?;
    if !has_end_header(&tail) {
        bail!("truncated RAR archive (end of archive not found)");
    }
    Ok(())
}

/// RAR5 の可変長整数（下位 7 ビットずつ、最上位ビットが継続）を読み、値とバイト数を返す
fn read_vint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0;
    for (i, &byte) in buf.iter().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// `tail` に RAR5 の終端ヘッダがあるか
///
/// ヘッダは「CRC32・ヘッダサイズ（vint）・ヘッダ本体」で、本体は種別（vint、終端は 5）から
/// 始まる。CRC32 はヘッダサイズから本体の終わりまでにかかる。UnRAR は終端ヘッダの後ろを
/// 読まないので、後ろに余分なデータ（パディングなど）があってもよい。
fn has_end_header(tail: &[u8]) -> bool {
    const HEAD_ENDARC: u64 = 5;
    (0..tail.len().saturating_sub(4)).any(|start| {
        let crc = u32::from_le_bytes(tail[start..start + 4].try_into().unwrap());
        let rest = &tail[start + 4..];
        let Some((size, size_len)) = read_vint(rest) else {
            return false;
        };
        // 壊れたデータでは size が巨大になりうるので、溢れないよう比べる
        let Some(header) = usize::try_from(size)
            .ok()
            .and_then(|size| size.checked_add(size_len))
            .and_then(|len| rest.get(..len))
        else {
            return false;
        };
        crc32fast::hash(header) == crc
            && read_vint(&header[size_len..]).is_some_and(|(kind, _)| kind == HEAD_ENDARC)
    })
}

/// アーカイブ内のパスを、展開先 `output_dir` の下のパスにする
///
/// 中身は自前で書き出すので（`unrar_with_progress` 参照）、UnRAR によるパスの正規化は効かない。
/// `..` や絶対パスで展開先の外に書き出さないよう、ここで弾く。
fn entry_dest_path(output_dir: &Path, name: &Path) -> anyhow::Result<PathBuf> {
    let is_safe = name.components().all(|c| match c {
        Component::Normal(_) | Component::CurDir => true,
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => false,
    });
    // 空や `.` だけの名前は、展開先ディレクトリ自体を指してしまう
    let has_name = name.components().any(|c| matches!(c, Component::Normal(_)));
    if !is_safe || !has_name {
        bail!("unsafe path in archive: {}", name.display());
    }
    Ok(output_dir.join(name))
}

/// rarファイルを解凍する
///
/// UnRAR にファイルを作らせず、各エントリの中身をメモリに読み出して自前で書き出す。
/// unrar クレートは Linux では展開先のフルパスを UnRAR にそのまま渡すため、UnRAR が
/// 展開先を知らず、ファイル参照・ハードリンクの参照先をカレントディレクトリから探してしまう
/// （無関係なファイルが出力に紛れ込む）。シンボリックリンクも作らないので、
/// リンクを経由して展開先の外に書き出すこともない。
///
/// ファイル参照・リンク（RAR4 のシンボリックリンクも含む）は、UnRAR が中身を展開せず空で返すので、
/// 元のサイズがあるのに空ならエラーにする。
///
/// # Arguments
///
/// * `rar_file` - 解凍するrarファイル
/// * `output_dir` - 解凍先のディレクトリ
/// * `pb` - 進捗表示に使うプログレスバー
pub fn unrar_with_progress<P1: AsRef<Path>, P2: AsRef<Path>>(
    rar_file: P1,
    output_dir: P2,
    pb: ProgressBar,
) -> anyhow::Result<()> {
    catch_panic(|| extract_entries(rar_file.as_ref(), output_dir.as_ref(), &pb))
}

fn extract_entries(rar_file: &Path, output_dir: &Path, pb: &ProgressBar) -> anyhow::Result<()> {
    pb.set_position(0);
    pb.set_length(validated_entry_count(rar_file)? as u64);

    // 分割 RAR・途中で切れた RAR5 は上の validated_entry_count で弾いている
    let mut archive = Archive::new(rar_file).open_for_processing()?;
    while let Some(header) = archive.read_header()? {
        let entry = header.entry();
        let dest = entry_dest_path(output_dir, &entry.filename)?;
        archive = if entry.is_directory() {
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("failed to create {}", dest.display()))?;
            header.skip()?
        } else {
            let name = entry.filename.display().to_string();
            let unpacked_size = entry.unpacked_size;
            let (data, next) = header
                .read()
                .with_context(|| format!("failed to extract {name}"))?;
            if data.is_empty() && unpacked_size > 0 {
                bail!("unsupported entry (file reference or link): {name}");
            }
            let parent = parent_dir(&dest);
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            std::fs::write(&dest, data)
                .with_context(|| format!("failed to write {}", dest.display()))?;
            next
        };
        pb.inc(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use indicatif::ProgressBar;

    use crate::test_util::fixture;

    #[test]
    fn test_get_file_count() {
        // 001.png, sub/, sub/002.txt, 日本語.txt
        assert_eq!(super::get_file_count(fixture("basic.rar")).unwrap(), 4);
    }

    #[test]
    fn test_unrar_with_progress() {
        for name in ["basic.rar", "solid.rar"] {
            let dir = tempfile::tempdir().unwrap();
            let pb = ProgressBar::hidden();
            super::unrar_with_progress(fixture(name), dir.path(), pb.clone()).unwrap();

            assert_eq!(pb.position(), 4, "{name}");
            assert_eq!(
                crate::test_util::list_names(dir.path()),
                ["001.png", "sub", "日本語.txt"],
                "{name}"
            );
            assert_eq!(
                std::fs::read(dir.path().join("sub/002.txt")).unwrap(),
                b"text"
            );
            assert_eq!(
                std::fs::read(dir.path().join("日本語.txt")).unwrap(),
                b"japanese"
            );
            image::open(dir.path().join("001.png")).unwrap();
        }
    }

    #[test]
    fn test_multi_volume_is_rejected() {
        for name in ["multi.part1.rar", "multi.part2.rar"] {
            let err = super::get_file_count(fixture(name)).unwrap_err();
            assert_eq!(
                err.to_string(),
                "multi-volume RAR archives are not supported"
            );

            let dir = tempfile::tempdir().unwrap();
            let result =
                super::unrar_with_progress(fixture(name), dir.path(), ProgressBar::hidden());
            assert!(result.is_err(), "{name}");
        }
    }

    #[test]
    fn test_encrypted_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let result =
            super::unrar_with_progress(fixture("encrypted.rar"), dir.path(), ProgressBar::hidden());
        assert!(result.is_err());

        // ヘッダまで暗号化したものは、件数取得（サイズチェック段）の時点でエラーになる
        assert!(super::get_file_count(fixture("encrypted_headers.rar")).is_err());
    }

    #[test]
    fn test_truncated_is_error() {
        // エントリの境界で切れた RAR も、途中までのエントリで成功させずにエラーにする
        let data = std::fs::read(fixture("basic.rar")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.rar");
        for len in [60, 150, 200, 245, data.len() - 1] {
            std::fs::write(&path, &data[..len]).unwrap();
            assert!(super::get_file_count(&path).is_err(), "{len}");
            let out = tempfile::tempdir().unwrap();
            let result = super::unrar_with_progress(&path, out.path(), ProgressBar::hidden());
            assert!(result.is_err(), "{len}");
        }
    }

    #[test]
    fn test_has_end_header() {
        // rar 7 が書く終端ヘッダ（CRC32・サイズ 3・種別 5・フラグ 4・終端フラグ 0）
        let end = b"\x1d\x77\x56\x51\x03\x05\x04\x00";
        assert!(super::has_end_header(end));
        assert!(super::has_end_header(&[b"data".as_slice(), end].concat()));
        // 終端ヘッダの後ろの余分なデータは許す
        assert!(super::has_end_header(&[end.as_slice(), &[0; 16]].concat()));
        assert!(!super::has_end_header(&end[..7]));
        assert!(!super::has_end_header(b""));
        // 巨大なヘッダサイズでも panic しない
        let huge_size = [&[0; 4][..], &[0xff; 9], &[0x01]].concat();
        assert!(!super::has_end_header(&huge_size));
    }

    #[test]
    fn test_not_a_rar_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = crate::test_util::touch_files(dir.path(), &["empty.rar"]).remove(0);
        assert!(super::get_file_count(&file).is_err());
    }

    #[test]
    fn test_catch_panic() {
        let err = super::catch_panic::<()>(|| panic!("boom")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported or corrupted RAR archive (RAR library error: boom)"
        );
        assert_eq!(super::catch_panic(|| Ok(1)).unwrap(), 1);
    }

    #[test]
    fn test_entry_dest_path() {
        let base = Path::new("/work");
        assert_eq!(
            super::entry_dest_path(base, Path::new("a/b.png")).unwrap(),
            Path::new("/work/a/b.png")
        );
        // 展開先の外を指すパスは弾く
        for name in ["../x", "a/../../x", "/etc/passwd", "", ".", "./"] {
            assert!(
                super::entry_dest_path(base, Path::new(name)).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn test_links_are_rejected() {
        // ファイル参照・ハードリンク・シンボリックリンクは中身を取り出せないのでエラーにする。
        // 参照先をカレントディレクトリから探したり、リンクを作ったりしない
        for name in ["file_ref.rar", "hard_link.rar", "symlink.rar"] {
            let dir = tempfile::tempdir().unwrap();
            let err = super::unrar_with_progress(fixture(name), dir.path(), ProgressBar::hidden())
                .unwrap_err();
            assert!(
                err.to_string()
                    .starts_with("unsupported entry (file reference or link): "),
                "{name}: {err}"
            );
            // リンクより前の通常のファイルだけが書き出され、リンクは作られない
            assert_eq!(
                crate::test_util::list_names(dir.path()),
                ["001.png"],
                "{name}"
            );
        }
    }
}
