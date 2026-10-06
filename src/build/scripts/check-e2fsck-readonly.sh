#!/bin/sh
set -eu

bin_dir=${1:?Usage: check-e2fsck-readonly.sh DIRECTORY_WITH_MKFS_EXT4_AND_E2FSCK}
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' 0

check_readonly() {
    image=$1
    expected=$2
    before=$(sha256sum "$image")
    status=0
    "$bin_dir/e2fsck" -fn "$image" || status=$?
    printf '%s\n' "$before" | sha256sum -c -
    if [ "$status" -ne "$expected" ]; then
        printf 'e2fsck returned %s; expected %s for %s\n' "$status" "$expected" "$image" >&2
        return 1
    fi
    printf 'e2fsck readonly: exit %s, SHA-256 unchanged\n' "$status"
}

clean="$test_dir/clean.ext4"
damaged="$test_dir/group0-checksum.ext4"
dd if=/dev/zero of="$clean" bs=1024 count=0 seek=65536
"$bin_dir/mkfs.ext4" -F -b 4096 -O metadata_csum "$clean"
check_readonly "$clean" 0
cp "$clean" "$damaged"

# A 4 KiB ext4 filesystem starts group 0's descriptor at byte 4096.
# The descriptor checksum starts at byte 30. Change only its lowest bit.
checksum_offset=4126
checksum_byte=$(od -An -tu1 -j "$checksum_offset" -N 1 "$damaged")
flipped=$((checksum_byte ^ 1))
printf '%b' "\0$(printf '%03o' "$flipped")" | \
    dd of="$damaged" bs=1 seek="$checksum_offset" count=1 conv=notrunc
check_readonly "$damaged" 4
