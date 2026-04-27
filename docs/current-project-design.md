# aShot 当前项目设计与实现原理

本文按当前重构后的代码状态整理项目设计。`aShot` 已从 GNOME Shell 覆盖层方案切换为 Portal 优先的系统截图方案：先调用系统截图能力生成图片，再打开 GTK/libadwaita 编辑窗口进行 Flameshot 风格标注与导出。

## 1. 项目定位

`aShot` 是一个面向 Linux 桌面的截图编辑工具，核心体验参考 Flameshot：

- `ashot gui` 调用系统截图区域选择。
- 选择区域后打开独立 GTK 编辑窗口。
- 编辑窗口提供文本、线条、箭头、画笔、矩形、圆形、标记笔、马赛克、模糊、计数器、实心遮挡等本地图片操作。
- 保存、复制、Pin 等动作由 Rust 应用和 `ashot-core` 渲染器完成。

GNOME Shell 扩展链路已移除；项目不再依赖 Shell 侧覆盖层、Shell DBus 服务或扩展安装脚本。

## 2. 模块组成

| 模块 | 责任 |
| --- | --- |
| `ashot-core` | 配置、文档模型、标注模型、撤销/重做、文件名模板、PNG 渲染 |
| `ashot-ipc` | Rust app DBus 常量、代理和跨进程结果类型 |
| `ashot-capture` | Portal 优先的系统截图封装 |
| `ashot-cli` | Flameshot 风格命令行入口 |
| `ashot-app` | DBus 服务、GTK 编辑器、设置窗口、Pin 窗口 |
| `packaging` / `flatpak` | Desktop、AppStream、图标和 Flatpak manifest |

## 3. 主流程

```text
ashot gui
  -> CLI 启动或连接 ashot-app --service
  -> 调用 io.github.ashot.App.CaptureArea
  -> ashot-app 通过 ashot-capture 调用系统 screenshot portal
  -> Portal 让用户选择区域并返回 file URI
  -> ashot-app 打开 GTK 编辑窗口
  -> 用户在窗口内标注和操作图片
  -> ashot-core 将基础图片和标注渲染为最终 PNG
```

`ashot full` 和 `ashot screen` 使用同一套 Portal 优先后端；当前 per-screen 指定和固定 geometry 输出仍是待补齐能力。

## 4. 编辑模型

`ashot-core::Document` 保存编辑状态：

- 基础图片尺寸。
- 标注列表。
- 当前选中标注。
- 当前工具。
- 缩放值。

标注类型包括：

- `Text`
- `Line`
- `Arrow`
- `Brush`
- `Rectangle`
- `Ellipse`
- `Marker`
- `Mosaic`
- `Blur`
- `Counter`
- `FilledBox`

每个标注拥有 UUID，支持 bounds、hit-test、translate；矩形类和端点类标注支持 resize。

## 5. 渲染原理

导出由 `ashot-core::render_document` 完成：

- 基础图片保持不可变。
- 标注独立存储。
- 保存时执行一次 rasterize。
- 线条类工具使用粗线采样绘制。
- 文本使用 `font8x8` 位图字体。
- 马赛克按块平均色回填。
- 模糊对局部区域做简单邻域平均。
- Marker 使用半透明颜色混合。

GTK 编辑器用 Cairo 做屏幕预览，最终输出以 `ashot-core` 渲染为准。

## 6. CLI

当前命令行已迁移为 Flameshot 风格：

```bash
ashot gui
ashot gui --path ~/Pictures/Screenshots --clipboard --pin
ashot full --delay 500
ashot screen --raw > screenshot.png
ashot launcher
ashot config
```

旧命令 `ashot capture ...`、`ashot open-settings`、`ashot pin ...` 已从 CLI 层删除。

## 7. 当前限制

- 上传功能不在本轮范围。
- 托盘/StatusNotifier 不在本轮范围。
- `--region`、`--last-region`、`--print-geometry` 目前返回不支持，后续需要桌面后端能力补齐。
- GTK 文本输入仍是 prompt dialog，后续应改为画布内联编辑。
- 复制 final action 在非 GTK raw/service-only 路径下仍需要继续完善。
