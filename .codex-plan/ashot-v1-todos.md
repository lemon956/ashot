# aShot v1 实现 TODO List

## Summary
- 目标：实现一个 `GNOME/Wayland only`、`Rust + GTK4/libadwaita`、`Flatpak 优先` 的原生截图工具，首版覆盖截图、编辑、保存/复制、钉图、设置、命令调用、可选顶部入口。
- 核心原则：截图阶段只走 `GNOME + xdg-desktop-portal` 原生链路；不依赖 X11；不以自绘全屏选区层作为主方案。
- 交付形态：单仓库 workspace，包含后台服务、CLI、编辑器、钉图窗、配置模块、Flatpak 打包与基础测试。

## Implementation TODO

### 1. 项目初始化
- 建立 Rust workspace。
- 创建 crate：
  - `ashot-app`：GTK/libadwaita 主程序与窗口管理。
  - `ashot-core`：配置、数据模型、图像处理、导出逻辑。
  - `ashot-capture`：portal 调用与截图结果接收。
  - `ashot-ipc`：DBus 接口与共享类型。
  - `ashot-cli`：命令行入口。
- 选定依赖：
  - `gtk4`
  - `libadwaita`
  - `gio/glib`
  - `zbus`
  - `serde`
  - `toml`
  - `image`
  - `resvg/cairo` 或等价绘制方案
- 配置代码规范：
  - `rustfmt`
  - `clippy`
  - 基础 CI 命令占位
- 定义应用标识：
  - App ID：`io.github.ashot.App`
  - DBus name：`io.github.ashot.App`
- 完成最小可启动窗口，确认 GTK4/libadwaita 在本地可运行。

### 2. 配置与目录规范
- 实现 XDG 路径解析：
  - 配置：`~/.config/ashot/config.toml`
  - 截图默认保存目录：默认 `~/Pictures/Screenshots`
- 定义配置结构：
  - `default_save_dir`
  - `filename_template`
  - `auto_copy`
  - `post_capture_open_editor`
  - `pin_after_save`
  - `default_tool`
  - `default_color`
  - `default_stroke_width`
- 实现：
  - 首次启动自动生成默认配置
  - 配置读取、写入、校验、回退默认值
- 实现文件命名模板：
  - 默认 `Screenshot_%Y-%m-%d_%H-%M-%S.png`

### 3. CLI 与调用入口
- 实现命令：
  - `ashot capture area`
  - `ashot capture screen`
  - `ashot capture window`
  - `ashot open-settings`
  - `ashot pin <image-path>`
- CLI 行为：
  - 若后台服务未启动，则自动激活主应用/DBus 服务
  - capture 命令通过 DBus 发起请求
- 输出约定：
  - 成功返回 0
  - 用户取消截图返回非崩溃错误码
  - 系统能力不可用时输出清晰错误信息

### 4. 后台服务与 DBus
- 定义 DBus 接口：
  - `CaptureArea`
  - `CaptureScreen`
  - `CaptureWindow`
  - `OpenSettings`
  - `OpenEditor(file_uri)`
  - `PinImage(file_uri)`
- 实现后台激活逻辑：
  - 主应用可在无主窗口时常驻
  - 接到 DBus 请求时拉起截图或窗口
- 实现单实例行为：
  - 避免多次启动产生多个后台进程
- 处理并发策略：
  - 同时仅允许一个 capture 会话
  - 重复请求返回“busy”错误或排队策略，v1 选 `busy`

### 5. Wayland 原生截图链路
- 封装 `xdg-desktop-portal` 截图请求。
- 支持模式：
  - 区域截图
  - 全屏截图
  - 窗口截图
- 处理 portal 返回：
  - 成功时拿到截图文件 URI
  - 取消时优雅结束
  - 权限/后端缺失时显示错误
- 将截图结果导入编辑器工作流。
- 确认 Flatpak 环境与非 Flatpak 环境都使用相同 portal 路线。
- 不实现基于全屏透明窗口的像素抓屏逻辑。

### 6. 编辑器基础框架
- 实现编辑窗口：
  - 顶部工具栏
  - 画布区域
  - 属性面板或次级工具栏
  - 底部操作区：保存、复制、钉图、取消
- 画布模型：
  - 底图 bitmap
  - 标注对象列表 overlay items
  - 当前选中对象
  - undo/redo 栈
- 基础交互：
  - 鼠标拖拽创建对象
  - 点击选中对象
  - 移动对象
  - Delete 删除
  - Esc 取消当前绘制
  - Ctrl+Z / Ctrl+Shift+Z 或 Ctrl+Y
- 缩放与平移：
  - 编辑器内画布缩放查看
  - 不影响导出像素结果

### 7. 标注工具实现
- Text 工具：
  - 文本输入
  - 颜色选择
  - 字重：`Regular / Semibold / Bold`
  - 字号预设：如 `12 / 16 / 20 / 28 / 36`
  - 可移动、可删除、可重新编辑文本
- Arrow 工具：
  - 颜色
  - 线宽：`2 / 4 / 8 / 12`
  - 箭头头部样式固定一种
- Brush 工具：
  - 自由绘制路径
  - 颜色
  - 线宽：`2 / 4 / 8 / 12`
- Rectangle 工具：
  - 空心矩形框
  - 颜色
  - 线宽：`2 / 4 / 8 / 12`
- Mosaic 工具：
  - 框选区域
  - 块大小：`8 / 16 / 24`
  - 导出前按区域像素化
- 统一对象属性模型：
  - `id`
  - `kind`
  - `bounds`
  - `stroke_color`
  - `stroke_width`
  - `text_style`
  - `mosaic_level`
- 实现对象命中测试与重绘。

### 8. Undo/Redo 与编辑状态
- 对以下操作全部纳入撤销栈：
  - 创建对象
  - 删除对象
  - 移动对象
  - 属性修改
  - 文本编辑
  - 马赛克区域变更
- 限制撤销栈容量，避免内存失控。
- 为大图场景优化：
  - 底图只保留一份
  - overlay 操作以命令对象或轻量快照记录

### 9. 导出、保存与剪贴板
- 实现最终栅格化导出：
  - 按底图尺寸渲染所有 overlay
  - 输出 PNG
- 保存流程：
  - 首次直接按默认目录+模板保存
  - 可“另存为”
- 剪贴板流程：
  - 保存后自动复制图片到剪贴板
  - 若保存失败则不报告成功
- 操作按钮：
  - `保存`
  - `复制`
  - `保存并关闭`
  - `钉在屏幕上`

### 10. 钉图窗口
- 实现独立 pin viewer 窗口：
  - always-on-top
  - 可拖动
  - 可缩放
  - 可关闭
  - 可从菜单恢复原始尺寸
- 缩放范围：
  - `25% - 400%`
- 行为约束：
  - v1 不做点击穿透
  - v1 不做桌面部件化
  - v1 不做窗口内继续编辑
- 支持从：
  - 编辑器保存后直接钉图
  - CLI `ashot pin <image-path>` 打开钉图

### 11. 设置窗口
- 实现设置页：
  - 默认保存路径选择
  - 文件名模板
  - 默认颜色
  - 默认线宽
  - 是否截图后自动打开编辑器
  - 是否自动复制到剪贴板
  - 是否保存后自动钉图
- 实现目录选择器与配置即时保存。
- 提供“恢复默认设置”。

### 12. GNOME 顶部轻量入口
- 主方案：
  - 应用无需顶部入口也可完整使用
- 可选增强：
  - 检测系统是否存在 `AppIndicator/StatusNotifier` 支持
  - 若存在则显示顶部菜单
- 顶部菜单内容：
  - 区域截图
  - 全屏截图
  - 窗口截图
  - 打开设置
  - 退出后台
- 若系统不支持托盘：
  - 不报错
  - 保持 CLI/启动器/快捷键可用
- 不实现 GNOME Shell 扩展版入口作为 v1 必做项。

### 13. Flatpak 打包与桌面集成
- 编写 Flatpak manifest。
- 配置必要权限与 portal 集成。
- 补齐桌面文件：
  - `.desktop`
  - appstream metadata
  - icon
- 验证从应用菜单启动、从命令行启动、从快捷键启动都可用。
- 为 GNOME 自定义快捷键提供命令示例文档。

### 14. 错误处理与稳定性
- 错误分类：
  - portal 不可用
  - 用户取消
  - 文件保存失败
  - 剪贴板失败
  - 配置损坏
- UI 行为：
  - 用户取消不弹惊扰性错误
  - 真正失败给出可理解提示
- 日志：
  - debug 日志可开关
  - release 默认低噪音
- 保证不会出现：
  - 截图后残留透明窗口
  - 多次调用后失去输入焦点
  - 崩溃导致后台进程僵死

### 15. 测试
- 单元测试：
  - 配置读写
  - 文件名模板
  - 导出合成
  - 马赛克算法
  - undo/redo 命令模型
- 集成测试：
  - CLI 参数解析
  - DBus 请求分发
  - 单实例/忙状态处理
- 手工验收清单：
  - GNOME Wayland 区域截图
  - GNOME Wayland 窗口截图
  - GNOME Wayland 全屏截图
  - 取消截图
  - 保存到默认目录
  - 自动复制剪贴板
  - 文字/箭头/画笔/矩形/马赛克
  - 撤销重做
  - 钉图缩放与拖动
  - Flatpak 包内运行
  - 有托盘扩展与无托盘扩展两种环境
- 大图测试：
  - 4K 截图编辑与导出不卡死
- 多屏测试：
  - 不同分辨率屏幕下截图与钉图正常

### 16. 文档
- README：
  - 产品说明
  - Wayland/GNOME 设计原则
  - 构建方式
  - 运行方式
  - CLI 用法
  - GNOME 快捷键绑定示例
- `docs/architecture.md`：
  - crate 职责
  - portal 链路
  - DBus 设计
  - 编辑器数据模型
- `docs/release-checklist.md`：
  - 打包
  - 权限验证
  - 手工验收

## Execution Order
1. 项目初始化、配置模块、CLI 骨架
2. DBus 单实例与后台服务
3. portal 截图链路打通
4. 编辑器基础框架
5. 五类标注工具
6. 导出、保存、剪贴板
7. 钉图窗口
8. 设置窗口
9. 顶部轻量入口
10. Flatpak 打包、测试、文档

## Public Interfaces
- CLI：
  - `ashot capture area|screen|window`
  - `ashot open-settings`
  - `ashot pin <image-path>`
- DBus：
  - `CaptureArea`
  - `CaptureScreen`
  - `CaptureWindow`
  - `OpenSettings`
  - `OpenEditor(file_uri)`
  - `PinImage(file_uri)`
- 配置文件：
  - `~/.config/ashot/config.toml`

## Acceptance Criteria
- 在 GNOME + Wayland 下，截图流程不依赖 X11，稳定完成区域/窗口/全屏截图。
- 截图后可完成文字、箭头、画笔、矩形、马赛克编辑。
- 支持默认保存目录、保存 PNG、复制剪贴板。
- 支持把图片钉成置顶浮窗并缩放。
- 用户可通过 GNOME 自定义快捷键绑定 `ashot capture area`。
- 无托盘扩展环境下仍能完整工作；有扩展时可附加顶部菜单入口。

## Assumptions
- 当前仍按 v1 范围执行，不把延时截图、模糊、编号标注、历史记录纳入必做。
- 顶部入口采用“可选增强”，不以 GNOME 托盘能力为硬依赖。
- 若后续退出 Plan Mode，实施时按上述执行顺序直接开始，不再额外做产品分叉决策。
