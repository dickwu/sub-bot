# Third-party notices / 第三方声明

sub-bot 本身以 `MIT OR Apache-2.0` 双许可证发布（见 [LICENSE-MIT](LICENSE-MIT) 与 [LICENSE-APACHE](LICENSE-APACHE)）。
本文件列出随二进制一起分发、需要单独署名的第三方组件。

## ripgrep library crates (BurntSushi/ripgrep)

内置 `grep` / `find` 工具在进程内嵌入了 ripgrep 的搜索引擎，直接链接以下来自
[BurntSushi/ripgrep](https://github.com/BurntSushi/ripgrep) 的库 crate（不调用外部 `rg` 可执行文件）：

| crate | 用途 |
|---|---|
| [`grep-searcher`](https://crates.io/crates/grep-searcher) | 按行搜索、上下文行、二进制检测 |
| [`grep-regex`](https://crates.io/crates/grep-regex) | 基于 Rust `regex` 的匹配器（smart case、整词、字面量、多行） |
| [`grep-matcher`](https://crates.io/crates/grep-matcher) | 匹配器接口（经 `grep-regex` / `grep-searcher` 引入） |
| [`ignore`](https://crates.io/crates/ignore) | 并行目录遍历，遵守 `.gitignore` / `.ignore` / 全局 gitignore，glob 覆盖与文件类型 |
| [`globset`](https://crates.io/crates/globset) | glob 匹配（`find` 的 pattern） |

These crates are dual-licensed under the MIT license or the Unlicense, at your option
(`Unlicense OR MIT`). sub-bot uses them under the terms of the MIT license, reproduced below.

```
The MIT License (MIT)

Copyright (c) 2015 Andrew Gallant

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## Other dependencies

其余依赖（tokio、serde、reqwest、regex、MCP 官方 Rust SDK `rmcp`（Apache-2.0）、rustls / webpki-roots、以及上述 crate 的传递依赖如 `bstr`、`memchr`、`walkdir`、`same-file`、
`encoding_rs` 等）均在各自的许可证下使用（以 MIT / Apache-2.0 为主，另有 Unlicense、BSD-3-Clause、ISC 等）；完整列表见 `Cargo.lock`，
可用 `cargo tree` 或 [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) /
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) 生成许可证清单。
