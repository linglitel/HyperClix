#!/usr/bin/env bash
# Start the v4l2loopback device used by the camera viewer.
set -Eeuo pipefail
ACTION=${1:-status}
NR=${2:-42}
DEV=/dev/video$NR
case "$ACTION" in
status)
    lsmod | grep -q '^v4l2loopback ' && echo 'v4l2loopback: loaded' || echo 'v4l2loopback: not loaded'
    ls -l "$DEV" 2>/dev/null || true
    ;;
start)
    if [[ -e $DEV ]]; then
        echo "Already present: $DEV"
        exit 0
    fi
    sudo modprobe v4l2loopback video_nr="$NR" exclusive_caps=1 card_label='HyperClix Camera'
    for _ in {1..20}; do
        [[ -e $DEV ]] && { echo "Ready: $DEV"; exit 0; }
        sleep 0.25
    done
    echo "v4l2loopback loaded but $DEV did not appear" >&2
    exit 1
    ;;
*)
    echo "Usage: $0 [status|start] [video-number]" >&2
    exit 2
    ;;
esac
