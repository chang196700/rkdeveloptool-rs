# 上游来源

- 项目：https://github.com/rockchip-linux/rkdeveloptool
- 固定提交：`304f073752fd25c854e1bcf05d8e7f925b1f4e14`
- 获取日期：2026-10-26；获取时远端 HEAD 与该提交一致。
- 上游版权：Fuzhou Rockchip Electronics Co., Ltd；相关源码保留 Seth Liu 的署名。
- 上游源码标记：`GPL-2.0+`，对应 SPDX `GPL-2.0-or-later`。
- 本仓库 `LICENSE` 与该提交的 `license.txt` 字节相同。

## 移植对应关系

| Rust 模块 | 上游行为来源 |
| --- | --- |
| `usb`、`device` | `RKScan.cpp`、`RKComm.cpp`、`RKDevice.cpp` 与 `main.cpp` 设备操作 |
| `boot` | `RKBoot.cpp`、`RKImage.cpp`、`boot_merger.h` 与 `main.cpp` 的打包和 IDB 构建 |
| `crypto` | `crc.cpp` 与 `main.cpp` 的 IDB 算法 |
| `image`、`partition` | `main.cpp`、`gpt.h` |
| CLI | `main.cpp` 的命令和关键结果字段 |

迁移采用源码逐模块翻译和 Rust 接口重写；未将原 C++ 程序作为运行时后端。USB 由 nusb 直接调用主机接口。

## 有意调整的行为

- 错误返回非零退出码，包括原工具忽略的 pack、unpack 和 tagspl 失败。
- 参数错误在枚举 USB 前报告，避免缺失参数越界。
- 修正 tagspl 使用文件名长度而非标签长度写文件的问题。
- 离线命令无需初始化 USB，也不自动在程序安装目录创建日志。
- 修正 sparse 擦除长度的字节/扇区混用，写入前检查 chunk、CRC 和容量。
- UBI 在 NAND 上要求起点和显式分区长度按擦除块对齐，防止擦除相邻分区。
- 以显式字节编解码代替 packed 结构强转；校验文件区间及运算溢出。
- Maskrom 特殊尾包保留发送字节；传输成功以 nusb 的结果表示，不复制上游发送一字节却检查返回零的判断。
- 保留命令和关键输出字段，终端控制序列及进度文本不承诺逐字节一致。Rust 版本号独立于上游版本号。

平台差异、格式检查和构建通过均不代表具体芯片或存储介质已完成实机验收。
