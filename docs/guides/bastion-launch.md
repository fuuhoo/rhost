# 从堡垒机打开（外部连接）

> description: 把 Rhost 配成 JumpServer 等堡垒机的 SSH / SFTP 客户端，用 ssh:// 与 sftp:// 链接拉起不保存的单通道会话
>
> created: 2026-11-02 12:40:19
>
> updated: 2026-11-02 14:05:00

## 1. 功能说明

堡垒机（JumpServer、Teleport、齐治等）通常允许管理员指定"外部 SSH / SFTP 客户端"：点击资产列表里的某个主机，堡垒机客户端拼出一条 `ssh://` 或 `sftp://` 链接并拉起本地程序。Rhost 兼容这一套调用方式，可以直接被当作 Xshell / WinSCP 的替代品。

被链接拉起时，Rhost 打开的是**外部连接**：

- 不进主机列表、不写配置文件、不写系统钥匙串；
- 链接里的密码只存在于内存，关掉标签即清除；
- 整条连接**只开一条通道**，不允许多开通道的堡垒机也能正常使用。

涉及的实现位置：`src-tauri/src/external.rs`（链接与命令行解析、投递）、`src-tauri/src/lib.rs`（单实例回调、协议注册、URL 事件订阅）、`src-tauri/src/ssh/session.rs`（单通道 / 仅 SFTP 会话）、`frontend/src/lib/externalLinks.ts` 与 `frontend/src/stores/session.ts`（不保存会话与门禁）。

## 2. 在堡垒机客户端里配置

把堡垒机设置中的"SSH 客户端程序 / SFTP 客户端程序"指向 Rhost 的**可执行文件绝对路径**，由堡垒机在其后追加链接参数。

| 平台 | 可执行文件路径 |
|---|---|
| macOS | `/Applications/rhost.app/Contents/MacOS/rhost` |
| Windows | 安装目录下的 `rhost.exe`（开始菜单快捷方式的"目标"即完整路径） |
| Linux | deb/rpm 安装后为 `/usr/bin/rhost`；AppImage 为其挂载后的可执行文件路径 |

> **macOS 禁止使用 `open -a rhost --args <链接>`**：`open` 不会把参数传给应用进程，链接会丢失。必须直接调用 `Contents/MacOS/` 下的可执行文件。

堡垒机拼接命令的常见四种形态，Rhost 全部识别：

```bash
# 1) 链接作为唯一的位置参数
rhost ssh://user:password@10.0.1.25:2222

# 2) Xshell 风格：-url 带链接，-newtab 指定标签名
rhost -url ssh://user:password@10.0.1.25:2222 -newtab 生产跳板

# 3) sftp:// 链接：直接打开 SFTP 文件管理，不开终端
rhost sftp://user:password@10.0.1.25:2222

# 4) WinSCP 风格：/sessionname= 指定标签名
rhost /sessionname=文件传输 sftp://user:password@10.0.1.25:2222
```

`ssh://` 链接打开终端标签，`sftp://` 链接打开 SFTP 面板（此时底部面板顶替终端占满工作区）。

## 3. 支持的参数与链接写法

| 项目 | 规则 |
|---|---|
| 协议 | 仅 `ssh` 与 `sftp`，大小写不敏感；其余协议静默忽略 |
| 用户名 | `user@` 部分；用**最后一个** `@` 切分主机地址，因此用户名可含 `@`（如域名账号 `user%40corp@example.com`） |
| 密码 | `user:password@host`；缺省或写成 `user:@host` 视为无密码，连接时弹框询问一次 |
| 特殊字符 | 用户名与密码按 URL 百分号转义解析，`%40` → `@`、`%3A` → `:` |
| 主机 | 支持 IPv4、域名与方括号 IPv6 字面量（`ssh://user@[fe80::1]:22`） |
| 端口 | 缺省（含 `host:` 空端口）为 `22`；范围 `1–65535`，`0` 与非数字判为非法 |
| 路径 / 查询 / 片段 | `sftp://user@host/path?x=1` 中 `://` 之后的路径段一律忽略，只取 authority |
| 值里混了参数 | 链接值中出现裸空格时截到空格之前：客户端把整串塞进一个参数（`-url "ssh://h -newtab"`）也能打开。URL 里真要带空格用 `%20` |
| 标签名 | `-newtab`、`-sessionname`、`-tabname`（Xshell / 通用写法）与 `/newtab`、`/sessionname=`（WinSCP 斜杠写法）同义；值可写作 `-url=值` 或 `-url 值` |
| 忽略的参数 | `-newwin`（本应用只有一扇窗口）、`/ini=nul`、`/newinstance` 等未知带值选项与未知开关一律忽略，不阻断解析 |
| 自由参数 | 不含 `://` 的位置参数（如 `--dev` 之类的调试参数）忽略 |

标签名的优先级：`-newtab` / `/sessionname=` 给的名称 > 别名栏手动重命名 > `user@host:port`。一次调用通常只有一条链接，名字会回填给该次所有未命名的链接。

## 4. 交给正在运行的实例

链接到达时若 Rhost 已在运行，不会启动第二个进程：

- **命令行拉起**：单实例守卫（`tauri-plugin-single-instance`）把新进程的 argv 转交给已运行实例，由它解析并打开标签；
- **系统 URL**（浏览器 / 堡垒机点 `ssh://` 链接）：macOS 由打包时写入 `src-tauri/Info.plist` 的 `CFBundleURLTypes` 声明协议，链接经系统事件送达；Windows / Linux 在启动时调用 `register_all()` 注册协议。

> **协议注册的副作用**：Windows 与 Linux 上注册会把 Rhost 设为**当前用户**的 `ssh://` / `sftp://` 默认处理程序（写当前用户注册表 / `xdg-mime default`，不改系统级配置）。macOS 无需运行时注册。若不希望 Rhost 成为默认处理器，请在系统的"默认应用"设置里改回。

无论哪种来源，窗口都会被带到前台。冷启动时链接先入后端队列，前端启动完成后经 `take_external_links` 领取，因此启动瞬间到达的链接不会丢。

## 5. 外部连接与常规会话的差异

外部连接不会留下任何痕迹，同时为了守住"一条连接一条通道"关闭了一批功能：

| 能力 | 常规会话 | 外部连接 |
|---|---|---|
| 主机列表 / 配置文件 | 保存 | 不进列表、不落盘 |
| 密码存储 | 系统钥匙串 | 仅内存，关标签即清除，**绝不写钥匙串** |
| 历史会话（首页"最近连接"） | 记录 | 不记录 |
| 重启后恢复标签 | 恢复 | 不恢复 |
| MOTD 欢迎面板、彩色提示符、init 脚本与环境变量 | 按全局设置 | 关闭（各自需要一条 exec 子通道并写远端 `/tmp`） |
| 主机指标采集（CPU / 内存 / 网络 / 进程 / 磁盘） | 左栏监控采集 | 不采集，左栏显示说明文案 |
| SFTP 文件管理 | 终端标签可另开 | `ssh://` 会话禁止；要用文件管理请点堡垒机的 `sftp://` 链接 |
| 端口转发 | 支持 | 入口隐藏，面板不可用 |
| AI 助手的只读工具（读文件 / 列目录 / 系统信息） | 走 exec 通道 | 后端拒绝，报错以 `EXTERNAL_SINGLE_CHANNEL:` 开头 |
| 复制会话、标签右键"打开「文件」面板（SFTP）" | 支持 | 菜单项禁用或隐藏 |

端口转发与 SFTP 之所以禁用：它们都要在同一条已认证连接上再开一条通道，而这类堡垒机的策略是"第二条通道直接断会话"。需要终端 + 文件管理两者时，从堡垒机分别点 `ssh://` 与 `sftp://` 两条链接，各自一条连接、一条通道。

主机密钥仍按 TOFU 校验：首次连接会弹指纹确认框，选"仅本次连接"可以不写 `known_hosts`。

## 6. 链接无法解析时

链接解析失败（缺主机名、端口越界、IPv6 未加方括号等）不会中断启动，也不会打开空标签：Rhost 弹一条带具体原因的提示，并落 `external.link_rejected` WARN 日志；成功入队的链接落 `external.link_open` INFO 日志，`targets` 字段只记 `user@host:port` 与协议，**不含密码**。非 `ssh` / `sftp` 协议的链接（如浏览器里点开的 `http://`）直接忽略，不产生提示。日志面板与 JSONL 文件的排查方式见[应用日志排查手册](./troubleshooting/applog-troubleshooting.md)。

## 7. 另见

- [快速连接](./quick-connect.md)：应用内输入 SSH 命令直连（保存主机）
- [主机与密钥管理](./host-management.md)：保存主机的凭据与钥匙串存储
- [SFTP 文件传输](./sftp.md)：常规会话的文件管理与拖拽上传
- [SSH 终端](./ssh-terminal.md)：终端能力总览
