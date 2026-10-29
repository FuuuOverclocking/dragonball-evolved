#!/bin/bash
# 在工作区根目录下准备只读参考资料：
# .references/{firecracker,cloud-hypervisor,qemu,linux,specs/,specs-pdf-to-txt/}。
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

# 把 specs/ 下每个 PDF 抽成同名 .txt，逐行前缀 p<页号>：grep 命中即得页码，
# 再用 pdftotext -layout -f N -l N 读该页原文。页号即 PDF 物理页。
# 只有 intel-sdm、intel-mp、PCIe Base 非 PDF 不可；ACPI、UEFI、virtio 请直接查
# specs/ 下的 .rst/.tex 源码，章节结构和交叉引用都在，比转出来的文本更准。
pdf_to_txt() {
    local dest=$1 pdf count=0
    if ! command -v pdftotext >/dev/null; then
        echo "pdftotext not found: install poppler-utils" >&2
        return 1
    fi
    mkdir -p "$dest" || return
    while IFS= read -r pdf; do
        echo "    $pdf"
        pdftotext -layout "$pdf" - |
            awk -v RS='\f' '{n=split($0,L,"\n"); for(i=1;i<=n;i++) print "p" NR "\t" L[i]}' \
            >"$dest/$(basename "$pdf" .pdf).txt" || return
        count=$((count + 1))
    done < <(find "$refs/specs" -name '*.pdf')
    # 规范下载失败时不要留下空目录，否则重跑会被当作已完成而跳过。
    ((count))
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

# 依赖上面的 PDF，必须最后执行。
run pdf_to_txt specs-pdf-to-txt

if ((${#failed[@]})); then
    echo "failed: ${failed[*]}" >&2
    exit 1
fi
echo "references ready: $refs"
