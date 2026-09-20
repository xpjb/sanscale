#!/usr/bin/env sh
set -eu

# The multilingual previews need broad system coverage. On Arch Linux:
#   pacman -S ttf-dejavu noto-fonts noto-fonts-cjk noto-fonts-emoji
# Other platforms need equivalent Latin, CJK, Indic, symbol, and color-emoji fonts.

rm -f \
  hello.png unicode_map.png unicode_map_zoom.png \
  emoji_board.png emoji_board_zoom.png emoji_flags.png \
  editor.png code-editor.png markdown-editor.png markdown-editor-stream.png

cargo run --example hello -- --dump
cargo run --example unicode -- --dump
cargo run --example emoji -- --dump
cargo run --example editor -- --dump
cargo run --example code-editor -- --dump
cargo run --example markdown-editor -- --dump
cargo run --example markdown-editor -- --dump --stream

mkdir -p gallery
install -m 0644 hello.png gallery/hello.png
install -m 0644 unicode_map.png gallery/unicode.png
install -m 0644 emoji_board.png gallery/emoji.png
install -m 0644 editor.png gallery/editor.png
install -m 0644 code-editor.png gallery/code-editor.png
install -m 0644 markdown-editor.png gallery/markdown-editor.png
install -m 0644 markdown-editor-stream.png gallery/markdown-editor-stream.png

rm -f \
  hello.png unicode_map.png unicode_map_zoom.png \
  emoji_board.png emoji_board_zoom.png emoji_flags.png \
  editor.png code-editor.png markdown-editor.png markdown-editor-stream.png

printf '%s\n' "updated gallery previews"
