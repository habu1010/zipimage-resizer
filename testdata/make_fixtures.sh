#!/bin/sh
# RAR のテスト用フィクスチャを作り直すスクリプト
#
# テストの実行時に rar コマンドを要求しないよう、生成した .rar はリポジトリにコミットしておく。
# 作り直すときは rar コマンド（apt install rar）を入れてから、このディレクトリで実行する:
#   sh make_fixtures.sh
#
# このディレクトリの下の一時ディレクトリに全部作ってから rename で上書きするので、
# 途中で失敗しても既存のフィクスチャは消えない。
set -eu

cd "$(dirname "$0")"
work=$(mktemp -d "$PWD/.fixtures.XXXXXX")
trap 'rm -rf "$work"' EXIT
src="$work/src"
links="$work/links"
out="$work/out"
mkdir "$src" "$links" "$out"

# 中身: 1x1 の PNG、サブディレクトリのテキスト、日本語名のファイル
mkdir "$src/sub"
echo 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC' \
    | base64 -d > "$src/001.png"
printf 'text' > "$src/sub/002.txt"
printf 'japanese' > "$src/日本語.txt"

# RAR 7 以降は RAR4 形式で作成できないため、RAR5 形式のみ
# -r: 再帰、-s: ソリッド、-p: パスワード（ファイルの中身だけ暗号化）、
# -hp: パスワード（ヘッダも暗号化）、-idq: 出力を抑える
(cd "$src" && rar a -r -idq "$out/basic.rar" ./*)
(cd "$src" && rar a -s -r -idq "$out/solid.rar" ./*)
(cd "$src" && rar a -p'secret' -r -idq "$out/encrypted.rar" ./*)
(cd "$src" && rar a -hp'secret' -r -idq "$out/encrypted_headers.rar" ./*)

# 分割 RAR: 無圧縮で、ボリュームサイズより大きいファイルを入れる
head -c 15000 /dev/urandom > "$work/big.bin"
(cd "$work" && rar a -m0 -v10k -idq "$out/multi.rar" big.bin)

# ファイル参照（-oi）・ハードリンク（-oh）・シンボリックリンク（-ol）: 001.png の後に、
# それを参照する 002.png を置く
cp "$src/001.png" "$links/001.png"
cp "$links/001.png" "$links/002.png"
(cd "$links" && rar a -oi:1 -idq "$out/file_ref.rar" 001.png 002.png)
rm "$links/002.png" && ln "$links/001.png" "$links/002.png"
(cd "$links" && rar a -oh -idq "$out/hard_link.rar" 001.png 002.png)
rm "$links/002.png" && ln -s 001.png "$links/002.png"
(cd "$links" && rar a -ol -idq "$out/symlink.rar" 001.png 002.png)

mv -f "$out"/*.rar .
