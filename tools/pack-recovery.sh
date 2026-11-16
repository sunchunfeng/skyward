#!/usr/bin/env bash
# 代码有改动后，重新编译并打出带时间戳的 Skyward recovery 镜像。
# 内核仍来自 --stock 指向的原厂 recovery，不会随代码变化。
#
# 用法：
#   tools/pack-recovery.sh --label OS4.0.4
#   tools/pack-recovery.sh --version OS4.0.4-latest-20261116170027
#   tools/pack-recovery.sh --stock /path/to/recovery.img --label OS4.0.4
#
# 产物默认写到 target/images/skyward-<代号>-<标签>-<年月日时分秒>.img
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

# 本机原厂 recovery 备份；可用环境变量 SKYWARD_STOCK_RECOVERY 覆盖。
default_stock="${SKYWARD_STOCK_RECOVERY:-$HOME/install_software/Android/stock-recovery/dada-OS4.0.4/recovery.img}"

stock=""
config="devices/dada.toml"
label=""
version=""
out_dir="target/images"

while [ $# -gt 0 ]; do
    case "$1" in
        --stock) stock=${2:?--stock 需要原厂 recovery 镜像路径}; shift 2 ;;
        --config) config=${2:?--config 需要设备配置路径}; shift 2 ;;
        --label) label=${2:?--label 需要版本标签，例如 OS4.0.4}; shift 2 ;;
        --version) version=${2:?--version 需要 Recovery 打包版本，例如 OS4.0.4-latest-20261116170027}; shift 2 ;;
        --out-dir) out_dir=${2:?--out-dir 需要输出目录}; shift 2 ;;
        -h|--help)
            sed -n '2,8p' "$0"
            exit 0
            ;;
        *) echo "不认识的参数：$1" >&2; exit 2 ;;
    esac
done

if [ -z "$stock" ] && [ -f "$default_stock" ]; then
    stock=$default_stock
fi
if [ -z "$stock" ] || [ ! -f "$stock" ]; then
    echo "请用 --stock 指向原厂 recovery 分区镜像，或把备份放到：" >&2
    echo "  $default_stock" >&2
    echo "示例：" >&2
    echo "  tools/pack-recovery.sh --label OS4.0.4" >&2
    exit 2
fi
if [ "$stock" = "$default_stock" ]; then
    echo "使用原厂 recovery 备份：$stock"
fi
if [ ! -f "$config" ]; then
    echo "找不到设备配置：$config" >&2
    exit 2
fi

# 未设置时用这台机器上已装好的 NDK r29。r30 的静态 libc 和当前 Rust 冲突。
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    default_ndk="$HOME/install_software/Android/ndk29/AndroidNDK14206865.app/Contents/NDK"
    if [ -f "$default_ndk/source.properties" ]; then
        export ANDROID_NDK_HOME="$default_ndk"
    else
        echo "请先设置 ANDROID_NDK_HOME，指向 Android NDK r29" >&2
        exit 2
    fi
fi

if ! command -v cargo-ndk >/dev/null 2>&1; then
    ndk_cargo="$HOME/install_software/Android/cargo-ndk/bin"
    if [ -x "$ndk_cargo/cargo-ndk" ]; then
        export PATH="$ndk_cargo:$PATH"
    else
        echo "找不到 cargo-ndk。可执行：cargo install cargo-ndk" >&2
        exit 2
    fi
fi

bash_bin="$root/tools/bash/out/bash"
omapi_bin="$root/tools/omapi-bridge/out/omapi-bridge"
if [ ! -f "$bash_bin" ]; then
    echo "缺少静态 Bash：$bash_bin" >&2
    echo "它不随 Rust 代码变化。需要时再执行 tools/bash/build.sh（要有 aarch64-linux-gnu-gcc）。" >&2
    exit 2
fi

omapi_args=()
if grep -q 'path = "/skyward/omapi-bridge"' "$config"; then
    if [ ! -f "$omapi_bin" ]; then
        echo "这份设备配置需要 OMAPI bridge：$omapi_bin" >&2
        echo "它不随 Rust 代码变化。macOS 上按 tools/omapi-bridge/build.sh 的步骤，把编译器路径改成 darwin 的 clang++。" >&2
        exit 2
    fi
    omapi_args=(--omapi "$omapi_bin")
fi

codename=$(sed -n 's/^codename = "\(.*\)"/\1/p' "$config" | head -1)
if [ -z "$codename" ]; then
    echo "配置里没有 codename：$config" >&2
    exit 2
fi

# cargo 的产物目录可能被 CARGO_TARGET_DIR 改到别处，所以向 cargo 问实际位置。
target_dir=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
if [ -z "$target_dir" ]; then
    echo "问不到 cargo 的产物目录" >&2
    exit 2
fi

echo "编译 recovery 程序（NDK：${ANDROID_NDK_HOME}）"
cargo ndk -t arm64-v8a --platform 30 build --release -p skyward
cargo build --release -p skyward-pack
RUSTFLAGS="-C target-feature=+crt-static" cargo ndk -t arm64-v8a --platform 30 build --release -p skyward-system-init --target-dir target/system-init

stamp=$(date +%Y%m%d%H%M%S)
mkdir -p "$out_dir"
name="skyward-${codename}"
if [ -n "$label" ]; then
    name="${name}-${label}"
fi
out="$out_dir/${name}-${stamp}.img"
if [ -n "$version" ]; then
    pack_version=$version
elif [ -n "$label" ]; then
    pack_version="${label}-${stamp}"
else
    pack_version=$stamp
fi

official=$(sed -n '/\[workspace.package\]/,/\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)
if [ -z "$official" ]; then
    echo "读不到 workspace 的 version（Cargo.toml [workspace.package]）" >&2
    exit 2
fi

echo "打包 $out"
"$target_dir/release/skyward-pack" recovery \
    --stock "$stock" \
    --binary "$target_dir/aarch64-linux-android/release/skyward" \
    --system-init target/system-init/aarch64-linux-android/release/system-init \
    --bash "$bash_bin" \
    ${omapi_args[@]+"${omapi_args[@]}"} \
    --config "$config" \
    --font resources/fonts/MiSansVF.ttf \
    --mono resources/fonts/KirkMono-Regular.ttf \
    --mono-license resources/fonts/KirkMono-OFL.txt \
    --pack-version "$pack_version" \
    --out "$out"

shasum -a 256 "$out"
{
    echo "Recovery 打包版本: $pack_version"
    echo "Skyward 官方版本: v$official"
} | tee "${out%.img}.version.txt"
echo "完成：$out"
