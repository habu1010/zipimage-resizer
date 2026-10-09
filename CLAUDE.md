# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## プロジェクト概要

zip でまとめられた画像（電子書籍・漫画アーカイブを想定）を、画像を WebP に再エンコード・縮小することで軽量化し、新しい zip として出力する CLI ツール。

```
zipimage-resizer [-d 出力先ディレクトリ] [--max-height 1800] [-k|--keep-mtime] <zipfile>...
```

- `-d/--output_dir` 未指定時は、元ファイル名に `_resized` サフィックスを付けたパスへ出力する（`determine_output_path`）。
- `-k/--keep-mtime` で出力 zip の最終更新日時を元ファイルに合わせる。

## 開発コマンド

```bash
cargo build              # デバッグビルド
cargo build --release    # リリースビルド（strip 有効）
cargo test               # テスト実行
cargo test test_append_suffix_to_filename   # 単一テスト実行
cargo clippy             # lint
cargo fmt                # フォーマット
```

## アーキテクチャ

### 4 段のパイプライン（[src/app.rs](src/app.rs)）

`run()` が 4 つのワーカースレッドを立ち上げ、`std::sync::mpsc` チャネルで繋いだパイプラインを構成する。各段は自身の `ProgressBar` を持ち、`MultiProgress` で同時表示される。

1. `spawn_size_check_thread` — zip のファイルサイズ ÷ 内包ファイル数で 1 ファイル平均サイズを求め、**500KB（`MIN_AVERAGE_FILE_SIZE`）超のものだけ**を下流に流す（既に十分小さい zip は処理しない）。
2. `spawn_zip_extraction_thread` — `tempfile::TempDir` に展開する。
3. `spawn_image_resizing_thread` — 展開ファイルを rayon で並列にリサイズ。**1 枚も変換されなかった zip は下流に送らない**（＝出力ファイルを作らない）。
4. `spawn_zip_creation_thread` — 出力先と同じディレクトリの一時ファイル（`*.zip.tmp`）に zip を書き、完了後に rename（`persist`）で出力先へ置き換える（途中で失敗・中断しても、書きかけの zip が出力先に現れず、既存のファイルも壊さないため）。置き換え前に `sync_all` する（電源断時に既存の出力が空ファイルに置き換わるのを防ぐ）。一時ファイルは既定で 0600 になるので、umask に従うパーミッションを明示している。失敗時の一時ファイルは drop で消えるが、Ctrl-C などの強制終了時は `.a_resized.zip.XXXXXX.tmp` が残る（展開用の一時ディレクトリが /tmp に残るのと同様、許容している）。

展開済みの一時ディレクトリを受け渡すチャネル（展開 → リサイズを作る `spawn_zip_extraction_thread`、リサイズ → zip 作成を作る `spawn_image_resizing_thread`）は、どちらも `sync_channel(MAX_PENDING_WORK_DIRS)` でバックプレッシャーをかけ、一時ディレクトリが際限なく溜まるのを防いでいる。上限はチャネルごとで、同時に存在しうる一時ディレクトリの数は定数のドキュメントコメントを参照。

`ProcessingContext` が段間で受け渡される単位で、`TempDir` を所有しているためコンテキストが drop されると展開先も自動削除される。

### エラー処理方針

- 個々のファイルの失敗（壊れた zip など）は `Reporter::report_error` でログに出して**次のファイルへ進む**。1 ファイルのために全体を止めない。`tx.send()` の失敗だけは下流が消えている合図なのでスレッドを終了させる。
- エラーメッセージは `MultiProgress::suspend` 経由で出す。そうしないとプログレスバーの描画と衝突して表示が崩れる。
- 失敗件数は `Reporter` が `AtomicUsize` で数え、1 件でもあれば `run()` の最後にエラーを返す（終了コードが非ゼロになる）。
- `run()` は 4 つのスレッドを**全て join してから**エラーを判定する。途中で `?` すると残りのスレッドが強制終了され、書きかけの zip が残るため。エラーは下流から順に探す（上流のエラーはチャネル切断による波及で、根本原因ではない）。
- 出力先ディレクトリは処理開始前に検証する（最終段まで進んでから失敗すると、それまでの展開・変換が無駄になる）。存在しない場合は作らずにエラーとする。
- 各入力の出力先は処理開始前に `plan_jobs` で一度だけ決め、`ZipJob`（入力と出力先の組）としてパイプラインに流す（後段で出力先を求め直さない）。その際、正規化したパスで、出力先が入力ファイルを上書きしないこと（`-d` に入力元と同じディレクトリを指定した場合や、再実行時に前回の `a_resized.zip` が glob に含まれた場合）と、出力先同士が重ならないこと（`-d` 指定時に別ディレクトリの同名 zip を渡した場合は先に指定した方を優先）を検証する。問題のある入力は報告して除き、残りは処理を続ける。同じファイルの重複指定は黙って 1 つにまとめる。なお出力先の予約はサイズチェックより前なので、平均サイズが `MIN_AVERAGE_FILE_SIZE` 以下で実際には出力されない入力も出力先を予約する（保守的な側に倒している）。
- ユーザーに表示するメッセージは英語で書く（コード内コメントは日本語）。

### 画像変換方針（[src/image_util.rs](src/image_util.rs), `resize_image_file_webp`）

- 変換対象外の判定: 既に WebP かつ 1MB 未満のファイルと、アニメーション画像（アニメーション GIF / APNG / アニメーション WebP。`image_util::is_animated`）はスキップする（戻り値 `None`）。静止画の WebP にすると先頭フレーム以外が失われるため。画像でない（形式を判定できない）ファイルと未対応の形式（`ImageError::Unsupported`）も黙って `None` を返す。形式は判定できたのにデコードできない壊れた画像は、エラーとして扱う。
- 個々の画像の変換に失敗した場合（読めない・壊れている・エンコードできないなど）は、その画像を元のまま残し、残りの画像の変換を続ける。失敗は `resize_image_files` の戻り値（`ResizeOutcome::failed`）で返し、zip ごとに 1 件のエラーとしてまとめて報告する（ディスクフルなどで大量に失敗しても表示が溢れないよう、個別のファイルは先頭 5 件まで）。zip は、失敗した画像だけ元の形式のまま出力される（終了コードは非ゼロ）。元画像は WebP の書き出しに成功してから削除するので、失敗しても元画像は失われない。
- デコードは `image_util::decode_with_orientation` で行い、EXIF の向き（Orientation）を画素に反映する（WebP には元の EXIF を引き継がないため）。
- `ReduceSize::reduce_size` は高さが上限（`--max-height`）を超えるときだけ Lanczos3 で縮小する（拡大はしない。ちょうど上限と同じ高さの画像もそのまま）。
- `ToWebPMemory::to_webp` は品質 95 → 85 → … → 15 と下げながらエンコードし、**1MB（`MAX_WEBP_SIZE`）以下に収まった最初の結果**を採用する。全滅時は品質 10 で確定。
- WebP エンコーダは RGB8 / RGBA8 しか受け付けないため、それ以外は透過情報があれば RGBA8、なければ RGB8 に変換してからエンコードする。
- 元ファイルは削除し、拡張子を `.webp` に変えて書き出す（zip 内のファイル名が変わる点に注意）。
- 変換は 2 段階で行う。並列フェーズでは同じディレクトリの一時ファイル（`*.webp.tmp`）に書き出し、書き込みに成功してから元ファイルを削除する。その後の逐次フェーズ（`rename_to_final_names`）で、実際に変換されたファイルだけに最終名を割り当てて rename する。
- 最終名は、ディレクトリに実在する名前（ディレクトリも含む）と大文字小文字を区別せずに照合して決める。変換対象外の既存の `001.webp` と衝突する場合は、元の拡張子を残した `001.png.webp`（さらに衝突すれば `001.png.1.webp` …）にする。`001.jpg` と `001.png` のように変換後の名前同士が衝突する場合は、ページ順が逆転しないよう全員を `001.jpg.webp` / `001.png.webp` にする。

### zip 入出力（[src/zip_util.rs](src/zip_util.rs)）

- 展開は `ripunzip` の `UnzipEngine`。進捗表示付きの `unzip_with_progress` は `single_threaded: true`（進捗カウントの整合性のため）。
- 圧縮は `zip` クレート。画像系拡張子（jpg/jpeg/png/webp）は `CompressionMethod::Stored`（無圧縮）にし、それ以外はデフォルト圧縮。既圧縮の画像を再圧縮しないための最適化。
- `zip_process_dir` はファイル → サブディレクトリの順で再帰し、パスは基準ディレクトリからの相対パスで格納する。
- ディレクトリ走査（`zip_process_dir` と `fs_util::scan_regular_files`）のエラーは無視せず返す。無視すると、読めなかったファイルが黙って出力 zip から抜け落ちるため。

### 未使用コード

`resize_image_file_jpg`（app.rs）は現状のパイプラインからは呼ばれていない（`#[allow(dead_code)]` 付き）。削除ではなく残されているので、参照する前に本当に必要か確認すること。
