# GNOME/Wayland 原生截图工具 v1 方案

## Summary
- 产品定位：`GNOME/Wayland only` 的原生截图工具，优先解决 `flameshot` 在 Wayland 下常见的权限、选区、输入抓取和兼容性问题。
- 技术选型：`Rust + GTK4 + libadwaita`，`Flatpak` 优先分发。
- 核心策略：截图阶段完全走 `GNOME/Wayland 原生能力`，不依赖 X11、不自己抢屏幕像素、不用脆弱的透明全屏覆盖层做截取。
- 运行形态：`轻量后台服务 + CLI + 编辑窗口`。主入口以命令调用和应用窗口为主；检测到 GNOME 扩展支持时，可附加 `AppIndicator` 顶部入口，但不把它作为唯一依赖。

## Key Changes
- 架构拆分为 4 个子系统：
  - `capture-service`：DBus 可激活后台服务，空闲时常驻但无高 CPU/内存占用。
  - `capture-cli`：供用户绑定 GNOME 全局快捷键，调用如 `ashot capture area`。
  - `editor-ui`：截图后打开编辑器，做标注、保存、复制、钉图。
  - `pin-viewer`：把最终图片以 always-on-top 浮窗形式钉在屏幕上，支持缩放和拖动。
- Wayland 截图链路固定为：
  - 优先使用 `xdg-desktop-portal` / GNOME 后端进行截图请求。
  - 区域选择使用 GNOME 原生交互式截图 UI。
  - 应用只接收截图结果文件，再进入编辑阶段。
  - 不实现自绘选区蒙层作为主路径，避免复现 flameshot 类问题。
- 编辑器工具定义：
  - `Text`：输入文字，支持字号、字重 `Regular/Semibold/Bold`、颜色。
  - `Arrow`：箭头，支持颜色、线宽 `2/4/8/12`。
  - `Brush`：自由画笔，支持颜色、线宽 `2/4/8/12`。
  - `Rectangle`：空心矩形框，支持颜色、线宽 `2/4/8/12`。
  - `Mosaic`：矩形区域马赛克，支持强度/块大小 `8/16/24`。
- 编辑模型固定为：
  - 底图为截图位图。
  - 标注层为独立对象列表，支持选中、移动、删除。
  - 支持 `Undo/Redo`，这是 v1 必带能力，否则标注体验会很差。
  - 导出时统一栅格化为最终图片。
- 钉图能力定义为：
  - 使用独立浮窗显示最终图像。
  - 窗口 `always-on-top`、可拖动、可缩放、可调整窗口大小。
  - 缩放范围 `25% - 400%`。
  - v1 不做点击穿透、不做桌面部件式贴图，避免 Wayland 下额外复杂度。
- 设置项与持久化：
  - 配置文件放在 XDG 配置目录，例如 `~/.config/ashot/config.toml`。
  - 至少包含：`default_save_dir`、`filename_template`、`auto_copy=true`、`post_capture_open_editor=true`、`pin_after_save=false`。
  - 默认行为为：`保存到默认路径 + 复制到剪贴板`。
- 对外接口固定为：
  - CLI：
    - `ashot capture area`
    - `ashot capture screen`
    - `ashot capture window`
    - `ashot open-settings`
    - `ashot pin <image-path>`
  - DBus：
    - `CaptureArea`
    - `CaptureScreen`
    - `CaptureWindow`
    - `OpenSettings`
    - `OpenEditor(file_uri)`
- GNOME 顶部入口策略：
  - 主方案不是“强依赖托盘”。
  - 若系统存在 `AppIndicator/StatusNotifier` 支持，则显示顶部菜单，提供“区域截图 / 全屏截图 / 打开设置 / 退出后台”。
  - 若不存在，则应用仍可通过启动器、快捷键、命令完整工作。

## Test Plan
- Wayland/GNOME 环境下验证：
  - 区域截图、窗口截图、全屏截图都能完成。
  - 用户取消截图授权或取消选区时，应用稳定返回，不残留异常窗口。
  - Flatpak 包内截图、保存、复制剪贴板都正常。
- 编辑器验证：
  - 文字、箭头、画笔、矩形、马赛克均可创建、修改、删除。
  - 颜色、线宽、字重切换立即生效。
  - Undo/Redo 覆盖所有编辑操作。
  - 导出后的图像与预览一致。
- 钉图验证：
  - 浮窗置顶、缩放、拖动、关闭正常。
  - 多屏环境下窗口行为稳定。
- 集成验证：
  - `ashot capture area` 适合绑定 GNOME 自定义快捷键。
  - 顶部入口存在与不存在两种环境都可正常使用。
  - 空闲后台服务资源占用保持很低。

## Recommended Extras
- `延时截图`：3s / 5s / 10s，实用性很高，且实现成本可控。
- `编号标注`：做步骤说明时比普通箭头更高频。
- `模糊`：与马赛克并列，部分用户更偏好模糊而不是像素化。
- `历史记录`：最近 N 张截图，方便重新钉图、再次复制、重新编辑。
- `快捷颜色栏`：预设常用颜色，避免每次打开拾色器。

## Assumptions
- 第一版以“稳定截图 + 稳定编辑 + 稳定钉图”为目标，不追求 KDE/Hyprland 通用性。
- GNOME 顶部入口采用“可选增强”而非“硬依赖”，以保证系统级轻量和兼容性。
- 为避免 Wayland 适配问题，截图选区交互交给 GNOME 原生截图 UI，而不是应用自己接管屏幕选择层。
- v1 导出格式默认只做 `PNG`，后续再扩展 JPEG/WebP。
