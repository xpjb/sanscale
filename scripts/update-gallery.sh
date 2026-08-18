#!/usr/bin/env sh
set -eu

# The multilingual previews need broad system coverage. On Arch Linux:
#   pacman -S ttf-dejavu noto-fonts noto-fonts-cjk noto-fonts-emoji
# Other platforms need equivalent Latin, CJK, Indic, symbol, and color-emoji fonts.

rm -f \
  hello.png paragraph.png unicode.png \
  unicode_map.png unicode_map_zoom.png \
  emoji_board.png emoji_board_zoom.png emoji_flags.png \
  editor.png

cargo run --example hello_png
cargo run --example paragraph
cargo run --example unicode
cargo run --example unicode_zoom -- --dump
cargo run --example emoji_zoom -- --dump
cargo run --example editor -- --dump

mkdir -p gallery
install -m 0644 hello.png gallery/hello.png
install -m 0644 paragraph.png gallery/paragraph.png
install -m 0644 unicode.png gallery/unicode.png
install -m 0644 unicode_map.png gallery/unicode_zoom.png
install -m 0644 emoji_board.png gallery/emoji_zoom.png
install -m 0644 editor.png gallery/editor.png

rm -f \
  hello.png paragraph.png unicode.png \
  unicode_map.png unicode_map_zoom.png \
  emoji_board.png emoji_board_zoom.png emoji_flags.png \
  editor.png

printf '%s\n' "updated gallery previews"
