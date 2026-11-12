#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
dest=${1:?Usage: build_initrd.sh NEW_OUTPUT_DIRECTORY}
test ! -e "$dest"
mkdir -p "$dest/initrd"/{bin,proc,sys,dev}
source_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
cp "${BUSYBOX:-/usr/bin/busybox}" "$dest/initrd/bin/busybox"
read -r -a extra_flags <<< "${CFLAGS:-}"
"${CC:-musl-gcc}" -static -O2 "${extra_flags[@]}" "$source_dir/marker.c" -o "$dest/initrd/marker"
cat > "$dest/initrd/init" <<'INIT'
#!/bin/busybox sh
set -e
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox ip link set lo up
/bin/busybox ip link set eth0 up
/bin/busybox ip addr add 10.231.218.2/24 dev eth0
/bin/busybox sleep 1
exec /marker
INIT
chmod +x "$dest/initrd/init"
(cd "$dest/initrd" && find . -print0 | cpio --null -o --format=newc) > "$dest/fixture-initrd.cpio"
sha256sum "$dest/fixture-initrd.cpio"
