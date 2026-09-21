# rs485-test 产品需求文档（PRD）

| 项 | 内容 |
|----|------|
| 产品名 | rs485-test |
| 版本 | 0.1.0 |
| 平台 | QCM6125（aarch64 Linux） |
| 总线 | RS485 半双工 |

## 1. 背景与问题

QCM6125 上的 RS485 为半双工：同一物理链路不能同时发和收，方向由独立 GPIO 切换。手册约定：

- GPIO **拉高** = 接收（RX）
- GPIO **拉低** = 发送（TX）

现场需要一个可静态部署、无需目标板安装运行时库的命令行工具，用来：

1. 与 PC / 对端设备互发数据，确认链路通断。
2. 做回显、压测、丢包与线路错误统计，量化通信质量。
3. 在无显示器场景用脚本拉起/停止，日志落盘。

本产品只覆盖 **板上测试代理**，不包含 PC 端 GUI 或协议栈实现。

## 2. 目标用户

| 角色 | 诉求 |
|------|------|
| 硬件/BSP 工程师 | 确认 UART 引脚、GPIO 方向、波特率、校验位是否正确 |
| 产测/现场工程师 | 一键交叉编译、拷贝三个文件到板子即可跑通 |
| 软件联调 | 用 echo / traffic / CRC 帧判断对端是否丢包、错包 |

使用前提：目标板有 root 权限，能写 `/sys/class/gpio` 并打开对应 tty 设备。

## 3. 产品目标

### 3.1 要做成什么

- 交叉编译出 **aarch64 musl 静态二进制**，拷到板子即可运行。
- 默认适配本项目硬件：`/dev/ttyHS3` + GPIO123 + 115200 + `N8N1`。
- 空闲时保持 RX，发送时按「拉低 → 等待 setup → 写串口 → tcdrain → 等待 hold → 拉高」切换。
- 提供 duplex / echo / send / recv / traffic / reverse / file-send / file-recv 工作模式。
- 可选按测试帧做 CRC、序号丢包/重复统计；利用内核 `TIOCGICOUNT` 报告帧错、奇偶、溢出。
- 提供 `scripts/start.sh` / `scripts/stop.sh` 做后台启停，停止后 GPIO 回到 RX。

### 3.2 非目标（本期不做）

- 不实现 Modbus / 自定义业务协议解析。
- 不提供图形界面、Web 页面或远程 RPC。
- 不自动探测串口/GPIO 编号（必须由参数或环境变量给出）。
- 不在目标板上现场编译（编译在 x86_64 主机完成）。
- 不支持 space/stick 校验（uarttest 的 `S`）。
- 不做多实例共享同一 GPIO/串口的仲裁。

## 4. 硬件与环境假设

| 项 | 默认值 | 可配置 |
|----|--------|--------|
| SoC | QCM6125 | — |
| 串口 | `/dev/ttyHS3` | `-d / --device` |
| 方向 GPIO | 123 | `-g / --gpio` |
| 波特率 | 115200 | `-b / --baud` |
| 帧格式 | `N8N1`（uarttest 风格） | `--format` |
| 操作系统 | Linux，sysfs GPIO | — |
| 权限 | root | — |
| 链接方式 | musl 静态 | 由 `scripts/compile.sh` 保证 |

方向时序（发送一帧）：

```
GPIO=1 (RX, 空闲)
    │
    ├─ set_tx: GPIO=0
    ├─ sleep tx_setup（默认 1 字节时间，下限 50 µs）
    ├─ write + flush + tcdrain
    ├─ sleep tx_hold（默认 2 字节时间，下限 50 µs）
    └─ set_rx: GPIO=1
```

进程退出（含 Ctrl-C、`Drop`）必须把 GPIO 拉回 RX，避免总线被卡在发送态。

## 5. 功能需求

### 5.1 全局参数

| 需求 ID | 描述 | 优先级 |
|---------|------|--------|
| F-CFG-01 | 可指定设备、波特率、uarttest 格式（`N8N1` 或 `115200N8N1`） | P0 |
| F-CFG-02 | 可指定方向 GPIO；首次使用时 export 并设为 output，默认拉高 RX | P0 |
| F-CFG-03 | `tx_setup_us` 控制拉低后、写串口前等待；未指定时按「1 字节时间」计算，下限 50 µs | P0 |
| F-CFG-04 | `tx_hold_us` 控制 tcdrain 后、拉高前等待；未指定时按「2 字节时间」计算，下限 50 µs | P0 |
| F-CFG-05 | `frame_idle_ms` 作为串口读超时，用于空闲组帧；默认 30 ms，内部下限 5 ms | P0 |
| F-CFG-06 | `--format` 支持校验 N/O/E、数据位 5–8、流控 Y/N、停止位 1/2；`S` 明确报错 | P1 |
| F-CFG-07 | `-q/--quiet` 不打印报文正文，只打统计；`--stats-interval-ms` 控制统计节流（默认 1000，0 表示每包） | P1 |
| F-CFG-08 | `--crc` 按测试帧校验并统计 CRC/丢包/重复/非测试帧 | P1 |

启动时打印当前配置一行（quiet 模式下不打印说明性 info，但退出时仍输出汇总统计）。

### 5.2 工作模式

未指定子命令时等价于 `duplex`。

| 需求 ID | 模式 | 行为 |
|---------|------|------|
| F-MODE-01 | duplex | 后台线程按空闲组帧收包并打印；主线程读 stdin，每行末尾补 `\n` 后发出。stdin 是终端且 EOF 则退出；stdin 是管道则发完后继续收，直到 Ctrl-C |
| F-MODE-02 | echo | 收到一帧后原样回发（半双工：先收完再切 TX） |
| F-MODE-03 | send | 发送一次文本 **或** `--file` 内容（二者互斥且必填其一）；`--stay` 时发完转入 recv |
| F-MODE-04 | recv | GPIO 保持 RX，只收不发 |
| F-MODE-05 | traffic | 连续发送带序号+CRC 的测试帧；`--count 0` 表示直到 Ctrl-C；可配 payload 长度与帧间隔 |
| F-MODE-06 | reverse（别名 test） | 启动后第一帧 1～8 字节立即倒序回发并冻结缓存；之后只比对是否与首帧相同，相同则直接发缓存，不同则不回发、不改缓存。读到数据后用 `FIONREAD` 抽干内核已到字节（最多 8），不等待 `frame_idle_ms`。有缓存且超过 6 秒无接收则把缓存倒序主动发送一次，并重置静默计时（持续静默则每 6 秒一次）。本模式串口读超时约 10ms，仅用于轮询空闲与 Ctrl-C |
| F-MODE-07 | file-send | 循环分片发送指定文件（默认 1024 字节/片、片间隔 20 ms）。启动时计算源文件 MD5 并写入 META；`--md5` 可选，传入则必须与本地计算结果一致。每轮发完等待文件级 ACK（默认 15 s），超时只记统计不中止。`--count 0` 直到 Ctrl-C |
| F-MODE-08 | file-recv | 按片号把实际收到的字节写入（不补 0，同片只留第一次）。缺片/乱序/重复/坏帧不中止本轮，打诊断日志后继续收。全部片到齐、空闲超时或新 META 时结束并 ACK。收齐后 `fsync` + `posix_fadvise(DONTNEED)` 再算 MD5。有 `--expect-md5` 则每轮必须匹配、通过也不留 first；未传入则以第一份收齐文件为基准，保留 `first-xfer{id}.bin`（半截/超时不记基准）。之后通过则删除 `.part`，失败则保留 `fail-xfer*.bin`、同名 `.log`（正文与 stdout 失败行一致）与可选 `-bad.bin`（`--max-fail-keep` 按 `got*.bin` 轮次计，删一轮时带走 `.log` / `-bad.bin`）。ACK 发送失败不退出。Ctrl-C 丢弃未完成 `.part` |

空闲组帧规则（duplex / echo / recv / file-send 的 ACK 等待 / file-recv）：读超时或读到 0 字节且缓冲区非空时，把已累积字节视为一帧；单帧上限 8192 字节。`reverse` 不走该规则。

### 5.3 测试帧协议（traffic / `--crc` / 文件传输载体）

用于压测与质量统计，不是业务协议。

| 字段 | 长度 | 说明 |
|------|------|------|
| Magic | 2 | `A5 5A` |
| Seq | 4 | 小端 u32，从 0 递增，允许回绕 |
| Len | 2 | 小端 u16，payload 字节数 |
| Payload | N | traffic 默认填充：`payload[i] = (seq as u8).wrapping_add(i)` |
| CRC | 2 | CRC-16/IBM（ARC），poly 反射 `0xA001`，初值 0，覆盖 seq+len+payload |

固定开销 10 字节。payload 必须 > 0，且 `payload + 10 ≤ 8192`。

接收侧 `--crc` 时：

- 无魔数：整段记为非测试帧。
- 有魔数：按长度切分，CRC 错或长度不完整记失败。
- 序号：首次帧建立期望值；超前记丢包数；落后（含重复）记 1 次重复。

### 5.3.1 文件传输 payload（file-send / file-recv）

仍走上述测试帧。payload 首字节为类型：

- META `0x01`：`xfer_id u32` + `file_size u64` + `chunk_count u32` + `md5[16]` + `name_len u8` + `name`
- DATA `0x02`：`xfer_id u32` + `chunk_idx u32` + `data`
- ACK `0x03`：`xfer_id u32` + `result u8`（0 通过 / 1 失败）+ `got_md5[16]`

每线上一帧只含一条消息。接收端按片号原样写入收到的 payload，不盲信 META 的 MD5。未传 `--expect-md5` 时，第一份收齐的文件保留为 `first-xfer{id}.bin` 并记为进程内基准。

### 5.4 统计与日志

每条日志行前缀为本地墙钟毫秒时间。

非 quiet：每帧打印

```
[RX|TX] len=… ascii="…" hex=…
```

不可打印字节在 ascii 中显示为 `.`。

统计行字段：

- `RX {包}包 {字节}字节  TX {包}包 {字节}字节`
- UART：`帧错` / `奇偶` / `溢出` 及相对 RX 字节的错误率；ioctl 不可用时打印 `UART计数=不可用`
- `--crc` 时追加：`CRC通过` / `失败` / `丢包` / `重复` / `非测试帧`，以及帧错误率、丢包率
- `file-send` / `file-recv` 追加：`FILE通过` / `FILE失败` / `ACK超时`。文件级事件（出错续收、记录基准并保留 first、通过删除、失败保留）即使 quiet 也打印。

进程正常结束前再打印一行带 `统计 ` 前缀的汇总。

### 5.5 启停脚本

| 需求 ID | 描述 |
|---------|------|
| F-OPS-01 | `scripts/compile.sh` 在 x86_64 主机交叉编译 `aarch64-unknown-linux-musl`，产物 `dist/rs485-test`，并检查尽量无动态 NEEDED |
| F-OPS-02 | `scripts/start.sh` 必须 root；export GPIO、拉高 RX、后台启动、写 pid 文件、stdout/stderr 追加到日志 |
| F-OPS-03 | `scripts/stop.sh` TERM → 等待 → 必要时 KILL，删除 pid，GPIO 拉回 RX |
| F-OPS-04 | start 通过环境变量覆盖设备、GPIO、波特率、模式、quiet、crc、traffic / file-send / file-recv 参数等 |

## 6. 非功能需求

| ID | 类别 | 要求 |
|----|------|------|
| N-01 | 部署 | 目标板不依赖 glibc 版本；release 剥离符号 |
| N-02 | 安全退出 | SIGINT/SIGTERM 与进程 Drop 都将 GPIO 置 RX |
| N-03 | 并发 | duplex 用一把互斥锁保护同一 tty，收发不会同时发生 |
| N-04 | 可观测 | 统计节流避免 115200 满载时刷屏拖垮终端 |
| N-05 | 失败可见 | 打开串口、export GPIO、参数互斥等错误用中文 context 退出码 1 |
| N-06 | 编译可重复 | 无工具链时自动拉取 Bootlin aarch64 musl；也可预置到 `tools/` |

## 7. 验收标准

1. 主机执行 `./scripts/compile.sh`，得到可执行的 `dist/rs485-test`（aarch64 ELF，宜为静态）。
2. 将 `rs485-test`、`start.sh`、`stop.sh` 拷到板子后，`MODE=recv` 能打开默认串口并把 GPIO123 置 1。
3. 对端发送任意字节，板上 recv 能按空闲间隔打出 RX 日志。
4. 板上 echo，对端发出的数据能原样收回。
5. duplex 下从板上 stdin 输入一行，对端收到该行加换行。
6. 一侧 `traffic --count 100`，另一侧 `recv --crc -q`，CRC 通过数与发送帧数一致（短线、无干扰时允许 0 丢包）。
7. 板上 `reverse`，对端反复发送同一段 1～8 字节，板上回倒序；超过 6 秒无接收则主动再发一次缓存倒序。
8. `stop.sh` 后进程不在、GPIO value 为 1。
9. 无 root 或设备不存在时，start 脚本在拉起前失败并给出原因。
10. 两板 `file-send` / `file-recv`：不传 MD5 时，收端第一份收齐的文件保留为 `first-xfer*` 并记下基准；之后相同内容删除、故意改内容或未收齐则保留 `fail-xfer*.bin` 与同名 `.log`。传入 `--expect-md5` 时第一轮不匹配也保留失败样本与日志，不改基准。缺片不中止本轮。

## 8. 风险与约束

- 半双工无冲突检测：两端同时发会破坏波形，echo/reverse/traffic/file-send 需约定主从（一侧 file-send，一侧 file-recv）。
- `file-recv` 未传 `--expect-md5` 时，若第一份收齐的文件其实已损坏，基准会锁错，后续正确文件会被当成失败。
- `file-recv` 收齐后会 fsync 并以 `posix_fadvise(DONTNEED)` 剔除该文件页缓存再回读。`--dir` 若在 tmpfs（常见于 `/tmp`）上，fadvise 可能无效，MD5 仍可能来自内存。
- `frame_idle_ms` 过小会把一帧拆成多段；过大则统计延迟增加。对端若连续发送无间隙，可能并成超大帧（上限 8192）。file-send 默认片间隔 20 ms，避免粘包切断。
- sysfs GPIO 在部分内核上已弃用；若板子只用 libgpiod 且无 sysfs，本期无法工作。
- `TIOCGICOUNT` 依赖驱动实现，部分 tty 返回不可用，此时不阻断收发。
- duplex 的 stdin 发送与 RX 线程抢同一把锁，高密度收包时发送会被短暂堵住，这是半双工的预期行为。

## 9. 文档与交付物

| 交付物 | 路径 |
|--------|------|
| 源代码 | `src/` |
| 交叉编译 | `scripts/compile.sh` |
| 板上启停 | `scripts/start.sh`、`scripts/stop.sh` |
| 使用说明 | `doc/使用说明.md` |
| 本 PRD | `doc/PRD.md` |
