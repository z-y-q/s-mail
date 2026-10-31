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

# 图标：用系统工具从程序化生成的 PNG 制作 icns（失败则跳过，用默认图标）。
# macOS 11+ 规范：内容占画布 ~82% 居中，四周透明边距。
# 图形：蓝渐变底 + 白信封（灰折盖 V 形）——一眼即邮件，字符字形（✉）
# 在小尺寸下发虚且与「邮件」关联弱，弃用
ICONSET="dist/AppIcon.iconset"
mkdir -p "$ICONSET"
if swift -e '
import AppKit
let size = CGFloat(1024)
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
let content = size * 0.824
let origin = (size - content) / 2

// 底：蓝色渐变圆角方块（上浅下深）
let bgRect = NSRect(x: origin, y: origin, width: content, height: content)
let bgPath = NSBezierPath(roundedRect: bgRect, xRadius: content*0.225, yRadius: content*0.225)
NSGradient(starting: NSColor(calibratedRed: 0.12, green: 0.58, blue: 1.0, alpha: 1),
           ending: NSColor(calibratedRed: 0.0, green: 0.38, blue: 0.86, alpha: 1))!
    .draw(in: bgPath, angle: -90)

// 信封主体：白色圆角矩形带投影
let envW = content * 0.68
let envH = content * 0.46
let envRect = NSRect(x: (size - envW) / 2, y: (size - envH) / 2, width: envW, height: envH)
let shadow = NSShadow()
shadow.shadowColor = NSColor(calibratedWhite: 0, alpha: 0.30)
shadow.shadowBlurRadius = content * 0.028
shadow.shadowOffset = NSSize(width: 0, height: -content * 0.014)
NSGraphicsContext.current?.saveGraphicsState()
shadow.set()
let envPath = NSBezierPath(roundedRect: envRect, xRadius: content*0.04, yRadius: content*0.04)
NSColor.white.setFill()
envPath.fill()
NSGraphicsContext.current?.restoreGraphicsState()

// 折盖：左上/右上角收拢到中线的 V 形（浅灰，信封的第二识别特征）
let inset = content * 0.018
let flap = NSBezierPath()
flap.move(to: NSPoint(x: envRect.minX + inset, y: envRect.maxY - inset))
flap.line(to: NSPoint(x: size / 2, y: envRect.midY - inset))
flap.line(to: NSPoint(x: envRect.maxX - inset, y: envRect.maxY - inset))
flap.close()
NSColor(calibratedRed: 0.84, green: 0.87, blue: 0.91, alpha: 1).setFill()
flap.fill()
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

# 图标缓存刷新：应用首次注册后改图标，Dock/Finder 会一直显示缓存的
# 通用图标（LaunchServices 不因包内容变化自动重绘）。touch 打散身份 +
# 重启 Dock 是图标开发期的标准做法（无破坏性，Dock 一秒重建）
touch "$APP"
killall Dock >/dev/null 2>&1 || true

echo "==> 完成：$APP"
codesign --verify "$APP" && echo "    签名校验通过"
echo "    启动：open $APP"
