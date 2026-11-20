#!/bin/sh
# Prints the `--scenario` arguments to run on an emulated link, and the corpus sizes it needs: the sizes
# keep a job inside its timeout (the slow links move little).
#
#   sh benchmarks/scenarios.sh <lan|wan|fat|lossy|slow|weak>
set -eu

pairs() {
    for name in "$@"; do
        printf -- '--scenario %s ' "$name"
    done
    printf '\n'
}

case "${1:?a link name}" in
lan)
    pairs connect info stat_x100_serial open_close_x50 read_1MiB_cold read_16MiB_cold read_256MiB_warm \
        write_16MiB exec_echo exec_out_16MiB checksum_16MiB remote_read_16MiB remote_info \
        raw_connect raw_down_256MiB raw_warm_down_256MiB raw_up_16MiB raw_warm_up_16MiB cpu_sha256_256MiB timeline_read_16MiB
    ;;
fat)
    pairs connect info stat_x20_serial open_close_x20 read_1MiB_cold read_64MiB_warm \
        write_16MiB exec_out_16MiB checksum_16MiB remote_read_16MiB \
        raw_connect raw_down_64MiB raw_warm_down_64MiB raw_up_16MiB raw_warm_up_16MiB cpu_sha256_256MiB timeline_read_16MiB
    ;;
wan | weak)
    pairs connect info stat_x20_serial open_close_x20 read_1MiB_cold read_16MiB_warm \
        write_1MiB exec_echo checksum_1MiB remote_read_1MiB \
        raw_connect raw_down_16MiB raw_warm_down_16MiB raw_up_1MiB raw_warm_up_1MiB cpu_sha256_256MiB timeline_read_1MiB
    ;;
lossy)
    pairs connect info stat_x20_serial open_close_x20 read_1MiB_cold read_4MiB_warm \
        write_1MiB exec_echo checksum_1MiB remote_read_1MiB \
        raw_connect raw_down_4MiB raw_warm_down_4MiB raw_up_1MiB raw_warm_up_1MiB cpu_sha256_256MiB timeline_read_1MiB
    ;;
slow)
    pairs connect info stat_x20_serial open_close_x20 read_256KiB_cold read_1MiB_warm \
        write_256KiB exec_echo checksum_256KiB remote_read_256KiB \
        raw_connect raw_down_1MiB raw_warm_down_1MiB raw_up_256KiB raw_warm_up_256KiB cpu_sha256_256MiB timeline_read_256KiB
    ;;
*)
    echo "unknown link" >&2
    exit 2
    ;;
esac
