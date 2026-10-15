# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## プロジェクト概要

zip または RAR でまとめられた画像（電子書籍・漫画アーカイブを想定）を、画像を WebP に再エンコード・縮小することで軽量化し、新しい zip として出力する CLI ツール。

```
zipimage-resizer [-d 出力先ディレクトリ] [--max-height 1800] [-k|--keep-mtime] <zip/rarファイル>...
```

- `-d/--output-dir` 未指定時は、元ファイル名に `_resized` サフィックスを付けたパスへ出力する（`determine_output_path`）。
- 出力は常に zip。拡張子 `.rar` / `.cbr` は中身によらず `.zip` にする（`a.rar` / `a.cbr` → `a_resized.zip`、`-d out` なら `out/a.zip`）。中身が RAR なら `.zip` / `.cbz` も `.zip` にし、それ以外の拡張子は名前の一部として残して `.zip` を足す（`Vol.1` → `Vol_resized.1.zip`）。中身が zip の `.zip` / `.cbz` などは従来どおり拡張子を変えない。
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

### モジュール構成

- [src/app.rs](src/app.rs) — CLI 引数、4 段のパイプラインの組み立て（各段の処理と、終了時のエラー件数の判定を含む）
- [src/plan.rs](src/plan.rs) — 出力先ディレクトリを検証し、各入力の形式と出力先を決め、上書きや出力先の重複を検証する（`plan_jobs` / `determine_output_path`）
- [src/reporter.rs](src/reporter.rs) — 進捗表示とエラー報告（`Reporter`）
- [src/convert.rs](src/convert.rs) — 展開済みの画像を WebP に変換する（`resize_image_files` / `resize_image_file_webp`）
- [src/webp_names.rs](src/webp_names.rs) — 変換後の WebP の出力名を衝突しないように決める（`rename_to_final_names`）
- [src/image_util.rs](src/image_util.rs) — 画像単体の処理（デコード・縮小・WebP エンコード・アニメーション判定）
- [src/archive.rs](src/archive.rs) — 入力の形式（zip / RAR）の判定（`ArchiveFormat::detect`）と、形式ごとの件数取得・展開への振り分け
- [src/zip_util.rs](src/zip_util.rs) — zip の展開・作成
- [src/rar_util.rs](src/rar_util.rs) — RAR のシグネチャによる判定（`is_rar`）と展開
- [src/fs_util.rs](src/fs_util.rs) — パス・ディレクトリ走査・一時ファイル・最終更新日時の小さなヘルパー
- `src/test_util.rs` — 複数モジュールのテストで共有するヘルパー（`#[cfg(test)]`）

### 4 段のパイプライン（[src/app.rs](src/app.rs)）

`run_pipeline()` が 4 つのワーカースレッドを立ち上げ、`std::sync::mpsc` チャネルで繋いだパイプラインを構成する。各段は自身の `ProgressBar` を持ち、`MultiProgress` で同時表示される。展開・リサイズ・zip 作成の段の進捗はアーカイブごとの件数なので、アーカイブを始めるたびに `start_archive` で戻し、段の完了は `finish_stage` で示す（件数が分かる前に失敗しても、前のアーカイブの完了表示が残らないように）。

1. `spawn_size_check_thread` — 入力アーカイブのファイルサイズ ÷ 内包ファイル数で 1 ファイル平均サイズを求め、**500KB（`MIN_AVERAGE_FILE_SIZE`）超のものだけ**を下流に流す（既に十分小さいアーカイブは処理しない。下限は `run_with` が `run_pipeline` に渡す）。
2. `spawn_extraction_thread` — `tempfile::TempDir` に展開する（zip / RAR は `ArchiveJob::format` で振り分け）。
3. `spawn_image_resizing_thread` — 展開ファイルを rayon で並列にリサイズ。**1 枚も変換されなかったアーカイブは下流に送らない**（＝出力ファイルを作らない）。
4. `spawn_zip_creation_thread` — 出力先と同じディレクトリの一時ファイル（`*.zip.tmp`、`fs_util::temp_file_beside`）に zip を書き、完了後に rename（`persist`）で出力先へ置き換える（途中で失敗・中断しても、書きかけの zip が出力先に現れず、既存のファイルも壊さないため）。置き換え前に `sync_all` する（電源断時に既存の出力が空ファイルに置き換わるのを防ぐ）。一時ファイルは既定で 0600 になるので、umask に従うパーミッションを明示している。失敗時の一時ファイルは drop で消えるが、Ctrl-C などの強制終了時は `.a_resized.zip.XXXXXX.tmp` が残る（展開用の一時ディレクトリが /tmp に残るのと同様、許容している）。

展開済みの一時ディレクトリを受け渡すチャネル（展開 → リサイズを作る `spawn_extraction_thread`、リサイズ → zip 作成を作る `spawn_image_resizing_thread`）は、どちらも `sync_channel(MAX_PENDING_WORK_DIRS)` でバックプレッシャーをかけ、一時ディレクトリが際限なく溜まるのを防いでいる。上限はチャネルごとで、同時に存在しうる一時ディレクトリの数は定数のドキュメントコメントを参照。

`ProcessingContext` が段間で受け渡される単位で、`TempDir` を所有しているためコンテキストが drop されると展開先も自動削除される。

### エラー処理方針

- 個々のファイルの失敗（壊れた zip など）は `Reporter::report_error` でログに出して**次のファイルへ進む**。1 ファイルのために全体を止めない。`tx.send()` の失敗だけは下流が消えている合図なのでスレッドを終了させる。
- エラーメッセージは `MultiProgress::suspend` 経由で出す。そうしないとプログレスバーの描画と衝突して表示が崩れる。
- 失敗件数は `Reporter` が `AtomicUsize` で数え、1 件でもあれば `run_with()` の最後にエラーを返す（終了コードが非ゼロになる）。
- `run_pipeline()` は 4 つのスレッドを**全て join してから**エラーを判定する。途中で `?` すると残りのスレッドが強制終了され、書きかけの zip が残るため。エラーは下流から順に探す（上流のエラーはチャネル切断による波及で、根本原因ではない）。
- 出力先ディレクトリは処理開始前に検証する（最終段まで進んでから失敗すると、それまでの展開・変換が無駄になる）。存在しない場合は作らずにエラーとする。
- 各入力の出力先は処理開始前に `plan_jobs` で一度だけ決め、`ArchiveJob`（入力・形式・出力先の組）としてパイプラインに流す（後段で出力先や形式を求め直さない）。その際、正規化したパスで、出力先が入力ファイルを上書きしないこと（`-d` に入力元と同じディレクトリを指定した場合や、再実行時に前回の `a_resized.zip` が glob に含まれた場合）と、出力先同士が重ならないこと（`-d` 指定時に別ディレクトリの同名 zip を渡した場合は先に指定した方を優先）を検証する。問題のある入力は報告して除き、残りは処理を続ける。同じファイルの重複指定は黙って 1 つにまとめる。なお出力先の予約はサイズチェックより前なので、平均サイズが `MIN_AVERAGE_FILE_SIZE` 以下で実際には出力されない入力も出力先を予約する（保守的な側に倒している）。
- ユーザーに表示するメッセージは英語で書く（コード内コメントは日本語）。

### 画像変換方針（[src/convert.rs](src/convert.rs), [src/webp_names.rs](src/webp_names.rs), [src/image_util.rs](src/image_util.rs)）

- 変換対象外の判定: 既に WebP かつ 1MB 未満のファイルと、アニメーション画像（アニメーション GIF / APNG / アニメーション WebP。`image_util::is_animated`）はスキップする（戻り値 `None`）。静止画の WebP にすると先頭フレーム以外が失われるため。画像でない（形式を判定できない）ファイルと未対応の形式（`ImageError::Unsupported`）も黙って `None` を返す。形式は判定できたのにデコードできない壊れた画像は、エラーとして扱う。
- 個々の画像の変換に失敗した場合（読めない・壊れている・エンコードできないなど）は、その画像を元のまま残し、残りの画像の変換を続ける。失敗は `resize_image_files` の戻り値（`ResizeOutcome::failed`）で返し、zip ごとに 1 件のエラーとしてまとめて報告する（ディスクフルなどで大量に失敗しても表示が溢れないよう、個別のファイルは先頭 5 件まで）。zip は、失敗した画像だけ元の形式のまま出力される（終了コードは非ゼロ）。元画像は WebP の書き出しに成功してから削除するので、失敗しても元画像は失われない。
- デコードは `image_util::decode_with_orientation` で行い、EXIF の向き（Orientation）を画素に反映する（WebP には元の EXIF を引き継がないため）。
- `ReduceSize::reduce_size` は高さが上限（`--max-height`）を超えるときだけ Lanczos3 で縮小する（拡大はしない。ちょうど上限と同じ高さの画像もそのまま）。
- `ToWebPMemory::to_webp` は品質 95 → 85 → … → 15 と下げながらエンコードし、**1MB（`MAX_WEBP_SIZE`）以下に収まった最初の結果**を採用する。全滅時は品質 10 で確定。
- WebP エンコーダは RGB8 / RGBA8 しか受け付けないため、それ以外は透過情報があれば RGBA8、なければ RGB8 に変換してからエンコードする。
- 元ファイルは削除し、拡張子を `.webp` に変えて書き出す（zip 内のファイル名が変わる点に注意）。
- 変換は 2 段階で行う。並列フェーズでは同じディレクトリの一時ファイル（`*.webp.tmp`）に書き出し、書き込みに成功してから元ファイルを削除する。その後の逐次フェーズ（`rename_to_final_names`）で、実際に変換されたファイルだけに最終名を割り当てて rename する。
- 最終名は、ディレクトリに実在する名前（ディレクトリも含む）と大文字小文字を区別せずに照合して決める。変換対象外の既存の `001.webp` と衝突する場合は、元の拡張子を残した `001.png.webp`（さらに衝突すれば `001.png.1.webp` …）にする。`001.jpg` と `001.png` のように変換後の名前同士が衝突する場合は、ページ順が逆転しないよう全員を `001.jpg.webp` / `001.png.webp` にする（拡張子のない `001` は、`001.webp` と同じ名前にならないよう `001.1.webp` にする）。

### RAR 入力（[src/archive.rs](src/archive.rs), [src/rar_util.rs](src/rar_util.rs)）

- 形式は拡張子ではなく先頭のシグネチャ（`Rar!\x1a\x07`、RAR 1.4 は `RE~^`）で判定する（中身が zip の `.cbr` などもあるため。`rar_util::is_rar`）。RAR でなければ zip として扱う。判定は `plan_jobs` で一度だけ行い、`ArchiveJob::format` に持たせる。
- 展開は `unrar` クレート（公式 UnRAR の C++ ソースを同梱して静的リンクするため、ビルドに C++ コンパイラが必要）。分割 RAR は非対応で、件数取得の時点（サイズチェック段）でエラーにする。エントリの境界で切れた RAR（ダウンロード途中など）は UnRAR では正常終了と区別できないので、RAR5 は末尾 64 バイト以内に終端ヘッダ（CRC 付き）があることを自前で確かめ（後ろの余分なデータは許す）、無ければ同じく件数取得の時点でエラーにする（`ensure_rar5_complete`）。RAR4 は終端ヘッダの無い古い形式（RAR 2.x）もあるので確かめない。パスワード付きはエラーとして報告される（中身だけ暗号化したもの（`-p`）は展開時、ヘッダまで暗号化したもの（`-hp`）は件数取得の時点）。
- `unrar` は Linux では展開先のフルパスを UnRAR にそのまま渡すため、UnRAR が展開先を知らない（ファイル参照・ハードリンクの参照先をカレントディレクトリから探す、パスの正規化が効かない）。そこで UnRAR にはファイルを作らせず、各エントリを `read()` でメモリに読み出して自前で書き出す。`..` や絶対パスのエントリは `entry_dest_path` で弾く。ファイル参照・ハードリンク・シンボリックリンクはエラーにする（リンクは一切作らない）。UnRAR はこれらの中身を展開せず空で返す（RAR4 のシンボリックリンクも同様）ので、「元のサイズがあるのに空」で検出する。`file_attr` はヘッダの生の値で、作成元の OS によって意味が違う（Windows なら Windows の属性）ため判定に使わない。
- unrar 0.5.8 は知らないエラーコード（辞書が大きすぎる場合の ERAR_LARGE_DICT など）で panic するので、`rar_util` の unrar を呼ぶ公開関数は `catch_panic` で panic をエラーに変える（1 ファイルのためにパイプライン全体を止めない）。その際、既定の panic フックが stderr に直接書いてプログレスバーを崩さないよう、`catch_panic` の中の panic ではフックの出力を抑える。
- `-d` に入力元と同じディレクトリを指定し、拡張子を変えた出力先（`a.rar` → `a.zip`）が既に存在する場合は処理しない（入力に含まれていなくても、同じ本の zip 版の原本かもしれないため）。入力や既存の出力がシンボリックリンクでも、リンク自体の置き場所で比べる（入力はリンク先のディレクトリも入力元とみなす）。`a.zip` と `a.rar` を両方渡した場合は、出力先の重複として先に指定した方だけを処理する。
- テスト用の RAR は `testdata/` にコミットしてある（テスト実行時に `rar` コマンドを要求しないため）。作り直すときは `rar` を入れて `sh testdata/make_fixtures.sh`。RAR 7 は RAR4 形式で作成できないので、フィクスチャは RAR5 のみ。

### zip 入出力（[src/zip_util.rs](src/zip_util.rs)）

- 展開は `ripunzip` の `UnzipEngine`。展開（`zip_util::extract`）は `single_threaded: true`（進捗カウントの整合性のため）。
- 圧縮は `zip` クレート。画像系拡張子（jpg/jpeg/png/webp）は `CompressionMethod::Stored`（無圧縮）にし、それ以外はデフォルト圧縮。既圧縮の画像を再圧縮しないための最適化。
- `add_dir_to_zip` は walkdir の 1 回の走査で全エントリを集め（件数は進捗表示に使う）、各ディレクトリで「ファイル（名前順）→ サブディレクトリ（名前順）」の深さ優先の順で格納する。パスは基準ディレクトリからの相対パス。
- ディレクトリ走査（`add_dir_to_zip` と `fs_util::scan_regular_files`）のエラーは無視せず返す。無視すると、読めなかったファイルが黙って出力 zip から抜け落ちるため。エラーに示すパスは、展開先からの相対パス（アーカイブ内のパス）にする（`fs_util::walk_error_in`。一時ディレクトリは報告する時点で消えているため）。1 エントリの追加に失敗した場合は、追加中だったエントリを `failed while adding <パス>` で示す。
