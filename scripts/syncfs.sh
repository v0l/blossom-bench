#!/bin/sh
set -eu
IMG=${BB_SYNC_IMG:-/tmp/blossom-bench-sync.img}
MNT=${BB_SYNC_MNT:-/tmp/blossom-bench-sync}
SIZE=${BB_SYNC_SIZE:-24G}
case "${1:-}" in
up)
    mountpoint -q "$MNT" && { echo "already mounted at $MNT"; exit 0; }
    [ -f "$IMG" ] || { truncate -s "$SIZE" "$IMG"; /sbin/mkfs.ext4 -q -F "$IMG"; }
    mkdir -p "$MNT"
    sudo mount -o loop,sync "$IMG" "$MNT"
    sudo chown "$(id -u):$(id -g)" "$MNT"
    echo "mounted $IMG at $MNT with sync"
    ;;
down)
    mountpoint -q "$MNT" && sudo umount "$MNT"
    rm -f "$IMG"
    ;;
*)
    echo "usage: $0 up|down" >&2
    exit 2
    ;;
esac
