#!/bin/bash
# 在工作区根目录下准备只读参考资料：.references/{firecracker,cloud-hypervisor,qemu,linux,specs/}。
# 已存在的条目跳过；要刷新某项，删除对应目录或文件后重跑。

set -uo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
refs=$root/.references
failed=()

# 先写入 <dest>.partial，成功后再改名，中断的下载不会在重跑时被当作已完成。
run() {
    local step=$1 name=$2
    local dest=$refs/$name
    if [[ -e $dest ]]; then
        echo "skip $name: already exists"
        return
    fi
    echo "==> $name"
    rm -rf "$dest.partial"
    mkdir -p "${dest%/*}"
    if "$step" "$dest.partial" "${@:3}" && mv "$dest.partial" "$dest"; then
        return
    fi
    rm -rf "$dest.partial"
    failed+=("$name")
}

clone() {
    local dest=$1 url=$2 ref=${3:-}
    git clone --depth 1 ${ref:+--branch "$ref"} "$url" "$dest"
}

download() {
    local dest=$1 url=$2
    curl --fail --location --retry 3 --proto '=https' --output "$dest" "$url"
}

unpack() {
    local dest=$1 url=$2 archive status
    archive=$(mktemp) || return
    download "$archive" "$url" && unzip -q "$archive" -d "$dest"
    status=$?
    rm -f "$archive"
    return $status
}

run clone firecracker https://github.com/firecracker-microvm/firecracker.git
run clone cloud-hypervisor https://github.com/cloud-hypervisor/cloud-hypervisor.git
run clone qemu https://gitlab.com/qemu-project/qemu.git
run clone linux https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git

# zip 内含 PDF、HTML 与 TeX 源码。
run unpack specs/virtio-1.4 https://docs.oasis-open.org/virtio/virtio/v1.4/cs01/virtio-v1.4-cs01.zip
# uefi.org 有 Cloudflare 质询，脚本无法下载；改用 UEFI Forum 发布的 reStructuredText 源码。
run clone specs/acpi-6.5 https://github.com/UEFI/ACPI-Specification-Release.git ACPI_6.5
run clone specs/uefi-2.11 https://github.com/UEFI/UEFI-Specification-Release.git UEFI_2.11
# Intel 的固定链接，总是指向最新的合订本。
run download specs/intel-sdm.pdf https://cdrdv2.intel.com/v1/dl/getContent/671200
# Intel 官网已下线，使用 MIT 6.828 的镜像。
run download specs/intel-mp-1.4.pdf https://pdos.csail.mit.edu/6.828/2018/readings/ia32/MPspec.pdf

if ((${#failed[@]})); then
    echo "failed: ${failed[*]}" >&2
    exit 1
fi
echo "references ready: $refs"
