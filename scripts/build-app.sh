#!/bin/sh
# s-mail macOS .app 打包（本地脚本，非 CI）：
#   sh scripts/build-app.sh
# 产物：dist/s-mail.app（ad-hoc 签名）
#
# 打包解锁：Dock 图标与名称、通知归属本应用（点击激活 s-mail 而非宿主
# 终端）、Finder 双击启动。通知点击跳转邮件需后续接 UNUserNotificationCenter。
set -e
cd "$(dirname "$0")/.."

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/version = "\(.*\)"/\1/')
APP="dist/s-mail.app"
echo "==> 构建 release（$VERSION）"
cargo build --release -p smail-ui

echo "==> 组装 bundle"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/s-mail "$APP/Contents/MacOS/s-mail"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>            <string>s-mail</string>
    <key>CFBundleDisplayName</key>     <string>s-mail</string>
    <key>CFBundleExecutable</key>      <string>s-mail</string>
    <key>CFBundleIdentifier</key>      <string>com.z-y-q.s-mail</string>
    <key>CFBundlePackageType</key>     <string>APPL</string>
    <key>CFBundleShortVersionString</key> <string>$VERSION</string>
    <key>CFBundleVersion</key>         <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>  <string>11.0</string>
    <key>NSHighResolutionCapable</key> <true/>
    <key>LSApplicationCategoryType</key> <string>public.app-category.productivity</string>
    <!-- 文件关联：.eml 双击用 s-mail 打开（v1 仅拉起应用，正文导入后续版本） -->
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key> <string>Email Message</string>
            <key>CFBundleTypeRole</key> <string>Viewer</string>
            <key>LSItemContentTypes</key>
            <array><string>public.mail-message</string></array>
        </dict>
    </array>
</dict>
</plist>
PLIST
printf 'APPL????' > "$APP/Contents/PkgInfo"

# 图标：用系统工具从程序化生成的 PNG 制作 icns（失败则跳过，用默认图标）
ICONSET="dist/AppIcon.iconset"
mkdir -p "$ICONSET"
if swift -e '
import AppKit
let size = CGFloat(1024)
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
NSColor(calibratedRed: 0.0, green: 0.48, blue: 1.0, alpha: 1).setFill()
NSBezierPath(roundedRect: NSRect(x: 0, y: 0, width: size, height: size), xRadius: size*0.22, yRadius: size*0.22).fill()
let para = NSMutableParagraphStyle(); para.alignment = .center
let attrs: [NSAttributedString.Key: Any] = [
    .font: NSFont.systemFont(ofSize: size*0.55, weight: .semibold),
    .foregroundColor: NSColor.white,
    .paragraphStyle: para,
]
let s = NSAttributedString(string: "\u{2709}", attributes: attrs)
s.draw(in: NSRect(x: 0, y: size*0.18, width: size, height: size*0.64))
img.unlockFocus()
guard let tiff = img.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
      let png = rep.representation(using: .png, properties: [:]) else { exit(1) }
FileManager.default.createFile(atPath: "dist/icon-1024.png", contents: png)
' 2>/dev/null && [ -f dist/icon-1024.png ]; then
    for s in 16 32 64 128 256 512; do
        sips -z $s $s dist/icon-1024.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null 2>&1
        cp "$ICONSET/icon_${s}x${s}.png" "$ICONSET/icon_${s}x${s}@2x.png" 2>/dev/null || true
    done
    sips -z 1024 1024 dist/icon-1024.png --out "$ICONSET/icon_512x512@2x.png" >/dev/null 2>&1
    if iconutil -c icns -o "$APP/Contents/Resources/AppIcon.icns" "$ICONSET" 2>/dev/null; then
        plutil -replace CFBundleIconFile -string AppIcon "$APP/Contents/Info.plist"
        echo "    图标已生成"
    fi
fi
rm -rf "$ICONSET" dist/icon-1024.png

echo "==> ad-hoc 签名"
codesign --force --sign - "$APP"

echo "==> 完成：$APP"
codesign --verify "$APP" && echo "    签名校验通过"
echo "    启动：open $APP"
