#!/usr/bin/env bash
#
# macOS 打包：编译 → 组装 .app → 生成图标 → ad-hoc 签名 → 打成 DMG。
#
#   ./scripts/package-macos.sh            # 有 x86_64 target 就做通用二进制，否则只做本机架构
#   ./scripts/package-macos.sh --no-dmg   # 只出 .app
#
# 产物：dist/Tramoing.app、dist/Tramoing-<version>.dmg
#
# 本地构建使用 ad-hoc 签名。面向其他用户发布时需配置 Developer ID 和公证。

set -euo pipefail
if [[ $(uname -s) != Darwin ]]; then
  echo "此脚本需要在 macOS 上运行" >&2
  exit 1
fi
cd "$(dirname "$0")/.."
ROOT=$PWD

APP_NAME="Tramoing"
APP_DISPLAY_NAME="Tramoing"
BIN="tramoing"
BUNDLE_ID="dev.guolongkang.trafficmonitor"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST="$ROOT/dist"
APP="$DIST/$APP_NAME.app"
MAKE_DMG=1

for arg in "$@"; do
  case "$arg" in
    --no-dmg) MAKE_DMG=0 ;;
    *) echo "未知参数：$arg" >&2; exit 1 ;;
  esac
done

if [[ -z "${VERSION}" ]]; then
  echo "读不到 Cargo.toml 里的 version" >&2
  exit 1
fi
echo "==> 版本 $VERSION"
TARGET_DIR=$(cargo metadata --format-version=1 --no-deps --locked |
  /usr/bin/plutil -extract target_directory raw -o - -)

# ---------- 1. 编译 ----------
# 两个 target 都在才做通用二进制。只有本机架构时也能出包，只是别的 CPU 跑不了。
TARGETS=()
if rustup target list --installed | grep -qx "aarch64-apple-darwin"; then
  TARGETS+=("aarch64-apple-darwin")
fi
if rustup target list --installed | grep -qx "x86_64-apple-darwin"; then
  TARGETS+=("x86_64-apple-darwin")
fi
if [[ ${#TARGETS[@]} -eq 0 ]]; then
  echo "没有安装任何 macOS target，先执行 rustup target add aarch64-apple-darwin" >&2
  exit 1
fi

for t in "${TARGETS[@]}"; do
  echo "==> 编译 $t (release)"
  cargo build --release --locked -p tray-app --target "$t"
done

# ---------- 2. 组装 .app ----------
echo "==> 组装 $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

if [[ ${#TARGETS[@]} -ge 2 ]]; then
  lipo -create \
    "$TARGET_DIR/aarch64-apple-darwin/release/$BIN" \
    "$TARGET_DIR/x86_64-apple-darwin/release/$BIN" \
    -output "$APP/Contents/MacOS/$BIN"
  echo "    通用二进制（arm64 + x86_64）"
else
  cp "$TARGET_DIR/${TARGETS[0]}/release/$BIN" "$APP/Contents/MacOS/$BIN"
  echo "    单架构（${TARGETS[0]}）"
fi
chmod +x "$APP/Contents/MacOS/$BIN"

sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
echo -n "APPL????" > "$APP/Contents/PkgInfo"
cp LICENSE "$APP/Contents/Resources/LICENSE"

# ---------- 3. 图标 ----------
if [[ ! -f assets/AppIcon.icns ]]; then
  echo "==> 生成 AppIcon.icns"
  ICON_WORK=$(mktemp -d)
  trap 'rm -rf "$ICON_WORK"' EXIT
  if [[ ! -f assets/icon-1024.png ]]; then
    rustc -O assets/gen-icon.rs -o "$ICON_WORK/icon-gen"
    "$ICON_WORK/icon-gen"
    sips -s format png assets/icon-1024.bmp --out assets/icon-1024.png >/dev/null
    rm -f assets/icon-1024.bmp
  fi
  ICONSET="$ICON_WORK/AppIcon.iconset"
  mkdir -p "$ICONSET"
  for sz in 16 32 128 256 512; do
    sips -z $sz $sz assets/icon-1024.png --out "$ICONSET/icon_${sz}x${sz}.png" >/dev/null
    sips -z $((sz * 2)) $((sz * 2)) assets/icon-1024.png --out "$ICONSET/icon_${sz}x${sz}@2x.png" >/dev/null
  done
  iconutil -c icns "$ICONSET" -o assets/AppIcon.icns
fi
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"

# ---------- 4. 签名 ----------
# ad-hoc 签名：不做的话 Apple 芯片上首次运行可能直接被拦。
echo "==> ad-hoc 签名"
codesign --force --deep --sign - --identifier "$BUNDLE_ID" "$APP"
codesign --verify --deep --strict "$APP" && echo "    签名校验通过"

# ---------- 5. DMG ----------
if [[ $MAKE_DMG -eq 1 ]]; then
  DMG="$DIST/Tramoing-$VERSION.dmg"
  STAGE="$DIST/.dmg-stage"
  echo "==> 打包 $DMG"
  rm -rf "$STAGE"
  mkdir -p "$STAGE"
  cp -R "$APP" "$STAGE/"
  # 拖拽安装的惯例：旁边放一个 Applications 软链
  ln -s /Applications "$STAGE/Applications"
  rm -f "$DMG"
  hdiutil create -quiet -volname "$APP_DISPLAY_NAME" -srcfolder "$STAGE" \
    -ov -format UDZO "$DMG"
  (cd "$DIST" && shasum -a 256 "$(basename "$DMG")" > "$(basename "$DMG").sha256")
  rm -rf "$STAGE"
  echo "    大小：$(du -h "$DMG" | cut -f1)"
fi

echo
echo "完成："
if [[ -d "$APP" ]]; then
  echo "  $APP  ($(du -sh "$APP" | cut -f1))"
fi
if [[ $MAKE_DMG -eq 1 ]]; then
  echo "  $DIST/$APP_NAME-$VERSION.dmg"
fi
