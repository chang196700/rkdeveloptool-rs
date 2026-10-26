# rkdeveloptool-rs

Rockchip [rkdeveloptool](https://github.com/rockchip-linux/rkdeveloptool) 的 Rust 移植，提供同步 Rust 库和 `rkdeveloptool` 命令行程序。USB 层使用纯 Rust 的 `nusb`，支持 Linux、macOS 和 Windows，无需安装 libusb 或编译原 C++ 项目。

移植基线为 `304f073752fd25c854e1bcf05d8e7f925b1f4e14`。许可证为 **GPL-2.0-or-later**，原许可证正文保存在 [LICENSE](LICENSE)，来源及行为差异见 [UPSTREAM.md](UPSTREAM.md)。

## 构建

使用当前稳定版 Rust（2024 edition）：

```sh
cargo build --release --locked
cargo install --path . --locked
```

直接依赖按 2026-10-26 crates.io 的最新稳定版本选择；`Cargo.lock` 固定实际解析的直接和传递依赖。更新依赖时先检查最新稳定版，再更新清单和锁文件。

## 主机配置

- Linux：将 [99-rk-rockusb.rules](99-rk-rockusb.rules) 安装到 `/etc/udev/rules.d/`，执行 `sudo udevadm control --reload-rules`，然后重新连接设备。规则通过 `uaccess` 授权本机登录用户；无桌面的主机可按本地用户组策略调整规则，或使用 root 运行。
- macOS：使用系统 IOKit，不依赖 Homebrew libusb。程序在设备未配置时选择兼容的 USB 配置；接口必须能够由用户程序占用。
- Windows：为 Rockchip 设备的 Maskrom 和 Loader 模式分别绑定 **WinUSB**。可通过 Zadig 安装；原厂专用 Rockusb 驱动不能直接供 nusb 使用。程序不安装或替换系统驱动。

## 命令

命令名不区分大小写，数值支持十进制、`0x` 十六进制和前导 `0` 八进制；LBA 单位是 512 字节。文件路径使用主机原生路径类型。

| 命令 | 用途 |
| --- | --- |
| `-h` / `--help` | 帮助；不带参数也显示帮助 |
| `-v` / `--version` | Rust 工具版本 |
| `ld` | 列出 Rockchip USB 设备 |
| `db [Loader]` | 按 ENTRY471、ENTRY472 顺序下载 Loader |
| `ul <Loader>` | 构建旧版或新版 IDB 并升级 Loader |
| `rl <BeginSec> <SectorLen> <File>` | 从 Flash 读取到文件 |
| `wl <BeginSec> <File>` | 写入 raw、Android sparse 或 UBI 镜像 |
| `wlx <PartitionName> <File>` | 根据 GPT 或 parameter 查找分区并写入 |
| `gpt <File>` | 从 parameter 生成 GPT，或调整 GPT 镜像后写入主备表 |
| `prm <File>` | 包装并写入 Rockchip parameter |
| `ppt` | 打印分区位置和名称 |
| `ef` | 按介质类型及设备能力擦除 Flash |
| `td` | 检查设备就绪状态 |
| `rd [subcode]` | 重启，subcode 默认为 0，范围 0..5 |
| `cs <storage>` | 切换存储并读取状态确认；1=eMMC、2=SD、9=SPI NOR |
| `rid` / `rfi` | 读取 Flash ID / Flash 信息 |
| `rci` / `rcb` | 读取芯片信息 / 设备能力 |
| `pack` | 使用工作目录的 `config.ini` 打包 Loader |
| `unpack <Loader>` | 解包 Loader 到工作目录 |
| `tagspl <tag> <SPL>` | 在 SPL 前添加标签，输出 `<SPL>.tag` |

除 `ld` 外，设备命令要求恰好连接一个匹配设备。`pack`、`unpack`、`tagspl`、帮助和版本不访问 USB。

省略 `db` 的 Loader 时，从可执行程序所在目录的 `config.ini` 读取 `loader=...`。相对 Loader 路径以工作目录为基准。`pack` 的分节配置和输入路径沿用上游规则；可使用上游 `cfg/` 下的配置作为起点。

```sh
rkdeveloptool ld
rkdeveloptool db RK356xLoader.bin
rkdeveloptool rfi
rkdeveloptool wl 0x8000 kernel.img
rkdeveloptool rd
```

raw 镜像尾部补零到完整扇区。sparse 镜像先校验全部 chunk，再擦除目标范围，并跳过 DONT_CARE 的写入；UBI 在 NAND 上执行预擦除。写入前检查镜像展开长度、分区边界和 Flash 容量。

## Rust 库

其他 Cargo 项目可使用路径依赖：

```toml
[dependencies]
rkdeveloptool = { package = "rkdeveloptool-rs", path = "../rkdeveloptool-rs" }
```

```rust
use rkdeveloptool::{list_devices, BootImage, Result};

fn main() -> Result<()> {
    let devices = list_devices()?;
    if let Some(info) = devices.first() {
        let mut device = info.open()?;
        device.set_progress_callback(|progress| {
            eprintln!("{}: {}/{}", progress.stage, progress.current, progress.total);
        });
        let sector = device.read_lba(0, 1)?;
        println!("read {} bytes", sector.len());
    }
    let loader = BootImage::open("Loader.bin")?;
    println!("{} boot entries", loader.entries.len());
    Ok(())
}
```

- `usb`：设备枚举、模式和主机位置，按枚举结果显式打开设备。
- `device`：同步设备命令、Flash 信息、Loader 操作和进度回调。
- `image`：镜像识别、sparse 校验、流式读写。
- `partition`：parameter、PARM 容器、GPT 主备表与分区操作。
- `boot`：BOOT/LDR、RKFW 元数据与分段提取、打包解包和 IDB。
- `crypto`：协议和文件格式使用的 CRC、RC4、BCH。

库函数返回统一的 `Error` / `Result`，不写终端。设备方法需要可变借用，操作按顺序执行。RKFW 签名和摘要元数据可以读取；未实现密码学签名验证。

## 验证状态

仓库遵循不新增测试文件的约束。构建检查使用：

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
cargo doc --no-deps --locked
```

CI 为 Linux、macOS、Windows 配置原生构建。离线格式检查使用临时目录和固定版本的上游程序作对照。具体已执行的检查与硬件验收状态见 [VALIDATION.md](VALIDATION.md)。
