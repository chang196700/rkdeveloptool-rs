# 验证记录

验证日期：2026-10-26。上游基线：`304f073752fd25c854e1bcf05d8e7f925b1f4e14`。

## 构建及静态检查

| 检查 | 结果 |
| --- | --- |
| Linux x86_64 `cargo build --release --locked --offline` | 通过，生成 `target/release/rkdeveloptool` |
| `cargo fmt --check` | 通过 |
| `cargo clippy --all-targets --locked --offline -- -D warnings` | 通过 |
| `cargo doc --no-deps --locked --offline` | 通过，无文档警告 |
| Windows GNU `cargo check --all-targets --locked --offline --target x86_64-pc-windows-gnu` | 通过；未进行 Windows 链接和运行 |
| macOS ARM64 `cargo check --all-targets --locked --offline --target aarch64-apple-darwin` | 通过；未进行 macOS 链接和运行 |
| `LICENSE` 与上游 `license.txt` 字节比较 | 相同 |

本次 Rust 工具链为 rustc/cargo 1.98.1。直接依赖已通过 crates.io API/注册表核实最新稳定版：anyhow 1.0.104、chrono 0.4.45、crc 3.4.0、nusb 0.2.9、uuid 1.29.0；实际版本见 `Cargo.lock`。

三平台原生构建工作流已配置；本次没有向远端推送或触发 GitHub Actions。

## CLI 与离线检查

在临时工作目录运行公开 CLI，确认帮助、版本、大小写命令、缺失参数、越界数字、非法八进制、未知命令及文件错误的结果与退出码。带空格的路径可用于 tagspl；输出恰为标签字节加原始 SPL。缺少 pack 配置时生成与上游相同的模板并返回错误。

本机 `ld` 返回“not found any devices!”和非零退出码。未执行任何设备写入或擦除。

## 与上游实现对照

原 C++ 工具在 `/tmp/rkdeveloptool-upstream` 构建。针对独立算法和格式，临时 C++ 调用器直接编译或提取固定提交的原始实现；Rust 调用器仅调用公开格式接口。

| 格式/算法 | 对照结果 |
| --- | --- |
| Loader pack | 普通四-entry 与大于 64 KiB 的 code entry 均匹配；仅排除生成时间及其影响的尾部 CRC |
| Loader unpack | 原始载荷及上游对齐填充匹配，保留上游大 entry 的 16 位 RC4 长度语义 |
| 默认 config.ini | 与上游生成的模板逐字节相同 |
| CRC32、CRC16、CRC-CCITT、GPT CRC32 | 与原 `crc.cpp` 相同 |
| RC4、BCH | 与 C++ 输出逐字节相同 |
| 旧 IDB | 与原 `MakeIDBlockData` 输出逐字节相同 |
| FlashHead IDB | 解码后按 2048 字节对齐的头部、数据、代码拼接结果匹配 |
| RKFW HI 扩展 | 大于 4 GiB 的稀疏文件可流式提取 Loader；无需将整个固件载入内存 |
| GPT 与容量调整 | 主备表与原 `create_gpt_buffer` / `update_gpt_disksize` 匹配；随机 GUID 及相关 CRC 单独归一化，显式 UUID 字节序相同 |
| PARM | 内容、长度、Rockchip CRC 和扇区填充逐字节相同 |
| Android sparse | RAW、FILL、DONT_CARE、CRC32、截断、长度不符及 CRC 错误均按预期解析或拒绝 |
| 畸形 Loader | 10,000 次重新计算有效 CRC 的头部/entry 变异未触发 panic |

临时复核材料位于 `/tmp/rkdeveloptool-format-check`、`/tmp/rkboot-validation`、`/tmp/rkboot-samples`，未新增仓库测试文件或 mocks。这些临时目录不是发行物，不能依赖其在后续环境中存在。

## 实机验收

初始移植时，以下项目在 Linux、macOS、Windows 上均未进行实机验证：设备枚举及权限、Maskrom ENTRY471/472 下载及特殊尾包、下载后重新枚举、Loader 查询、存储切换、指定测试区域写入回读、分区表写入、Loader 升级、介质擦除与重启。后续 Windows 验证见下文。

上述静态及离线结果不表示具体芯片、ROM 版本或存储介质已经完成硬件验收。

## Windows 原厂驱动实机验证

后续于 2026-10-26 在 Windows x86_64 上完成以下验证，补充上述初始移植记录。设备 VID/PID 为 `2207:350d`，保留 Fuzhou Rockchip `Rockusb 5.11.0.0` 驱动；使用 Rust 1.98.1 MSVC 原生构建。

- `rockusb-driver` 脱离 Workspace 独立进行 Windows 目标编译通过；依赖只有 `windows-sys` 和其 `windows-link`。
- Workspace 的 Linux 构建、Clippy、文档及 Windows GNU、macOS ARM64 交叉检查通过。
- 使用 `rk3562_spl_loader_v1.09.107.bin` 执行 `db`，RAM 下载成功；随后设备重新枚举，`td`、`rid`、`rfi`、`rci`、`rcb` 均通过。
- Flash ID 返回 `EMMC `，Flash 容量为 15,269,888 个扇区（7456 MiB）。
- 实机发现元信息查询的成功 CSW 包含大端响应长度；完整数据已收到。修正仅限此类读取的校验规则。
- 临时调用公开 `read_lba(0, 2049)`，结果与分开的 2048 扇区及 1 扇区读取完全一致，验证原厂驱动的 1 MiB 分块边界。
- 临时调用独立 crate 的 100 ms 超时读取，约 110 ms 后取消返回，随后设备就绪查询仍成功。

以上调用器及构建目录均位于临时目录，未新增仓库测试。未替换 Windows 驱动，未执行 Flash 写入、擦除或升级 Loader。Linux、macOS、WinUSB 后端的实机操作以及其余破坏性操作仍未验收。
