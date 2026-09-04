#!/usr/bin/env bash
# Time-to-first-pixel A/B harness for fire — the shell twin of scripts/ttfp.ps1, and the same
# method, so numbers taken with either are taken the same way.
#
# Launches two builds alternately on each test image, N times each, and reports median / mean / sd
# of the milliseconds from *kernel process creation* to the first image-bearing present. The binary
# measures itself: with FIRE_TTFP_OUT set it writes that number on its first image frame and exits
# (crates/fire/src/ttfp.rs). Nothing here times the child from outside — a `time` around the launch
# would measure the harness's own fork/exec too.
#
# The A/B runs are interleaved and the order flips every iteration. The spread between launches is
# a couple of ms, which is the same order as the effect usually being measured, so "which build
# happened to run second" would otherwise dominate.
#
#   scripts/ttfp.sh -A ../fire-main/target/release/fire -B target/release/fire \
#       -N 12 --csv out.csv img/small.png img/big.png
#
# Written for the bash macOS actually ships (3.2), not a Homebrew one — a measurement harness that
# only runs on the machine that installed extra tools is not much of a harness.
#
# A and B may be a bare binary or a .app bundle; a bundle is launched through its executable rather
# than through `open`, because `open` returns immediately and hands the arguments to Launch
# Services, so there would be no child to wait on. That also keeps both sides of an A/B comparable
# when only one of them is bundled.
set -euo pipefail

a=""; b=""; n=12; label_a=A; label_b=B; csv=""; images=()

usage() { sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        -A|--a)      a="$2"; shift 2 ;;
        -B|--b)      b="$2"; shift 2 ;;
        -N|--n)      n="$2"; shift 2 ;;
        --label-a)   label_a="$2"; shift 2 ;;
        --label-b)   label_b="$2"; shift 2 ;;
        --csv)       csv="$2"; shift 2 ;;
        -h|--help)   usage; exit 0 ;;
        -*)          echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
        *)           images+=("$1"); shift ;;
    esac
done

if [[ -z $a || -z $b || ${#images[@]} -eq 0 ]]; then
    echo "need -A <build> -B <build> and at least one image" >&2
    usage >&2
    exit 2
fi

# The executable to run: inside a .app bundle it is Contents/MacOS/<CFBundleExecutable>.
resolve() {
    local p="$1"
    if [[ -d "$p" && "$p" == *.app ]]; then
        local exe
        exe="$(plutil -extract CFBundleExecutable raw -o - "$p/Contents/Info.plist")"
        p="$p/Contents/MacOS/$exe"
    fi
    [[ -x "$p" ]] || { echo "not an executable: $p" >&2; exit 2; }
    # Absolute, so a bundle's own path lookups do not depend on the shell's cwd.
    (cd "$(dirname "$p")" && printf '%s/%s\n' "$PWD" "$(basename "$p")")
}

a="$(resolve "$a")"
b="$(resolve "$b")"

out="$(mktemp -t fire-ttfp)"
trap 'rm -f "$out"' EXIT

# One launch. Prints the milliseconds it reported.
launch() {
    local exe="$1" image="$2"
    rm -f "$out"
    # A timeout so a build that never presents an image frame fails the run instead of hanging it.
    # `perl` is on every Mac; `timeout` is not (it is GNU coreutils).
    # The child's own stdout/stderr would interleave with the table; the stamp comes back in a
    # file, so nothing useful is being thrown away.
    if ! FIRE_TTFP_OUT="$out" perl -e 'alarm shift; exec @ARGV' 30 "$exe" "$image" >/dev/null 2>&1; then
        echo "launch of $exe on $image failed or timed out" >&2
        exit 1
    fi
    [[ -s "$out" ]] || { echo "$exe exited without writing the stamp" >&2; exit 1; }
    local ms
    ms="$(tr -d '[:space:]' < "$out")"
    # A NaN is what ttfp.rs writes when it has no process-creation clock; never average it in.
    [[ "$ms" =~ ^[0-9]+(\.[0-9]+)?$ ]] || { echo "$exe reported '$ms', not a measurement" >&2; exit 1; }
    printf '%s\n' "$ms"
    # Let the previous window's teardown settle before the next launch competes with it.
    sleep 0.15
}

# median / mean / sd / min / max of the numbers on stdin.
summarize() {
    sort -g | awk -v label="$1" '
        { v[NR] = $1; sum += $1 }
        END {
            if (NR == 0) exit
            mid = int(NR / 2)
            median = (NR % 2) ? v[mid + 1] : (v[mid] + v[mid + 1]) / 2
            mean = sum / NR
            for (i = 1; i <= NR; i++) { d = v[i] - mean; ss += d * d }
            sd = (NR > 1) ? sqrt(ss / (NR - 1)) : 0
            printf "  %-8s median %7.1f ms   mean %7.1f   sd %5.1f   min %7.1f   max %7.1f   (n=%d)\n",
                   label, median, mean, sd, v[1], v[NR], NR
        }'
}

[[ -n $csv ]] && echo "image,iter,build,ms" > "$csv"

for image in "${images[@]}"; do
    [[ -f "$image" ]] || { echo "no such image: $image" >&2; exit 2; }
    printf '\n== %s (%s bytes) ==\n' "$(basename "$image")" "$(stat -f%z "$image")"

    # One warm-up launch per build, so the file cache holds the binary and the image and the first
    # measured iteration is not the only cold one.
    launch "$a" "$image" > /dev/null
    launch "$b" "$image" > /dev/null

    res_a=(); res_b=()
    for ((i = 0; i < n; i++)); do
        if (( i % 2 == 0 )); then
            res_a+=("$(launch "$a" "$image")")
            res_b+=("$(launch "$b" "$image")")
        else
            res_b+=("$(launch "$b" "$image")")
            res_a+=("$(launch "$a" "$image")")
        fi
        # bash 3.2 has no negative array subscripts.
        last_a="${res_a[$((${#res_a[@]} - 1))]}"
        last_b="${res_b[$((${#res_b[@]} - 1))]}"
        printf '  %2d: %s %7.1f   %s %7.1f\n' "$((i + 1))" "$label_a" "$last_a" "$label_b" "$last_b"
        if [[ -n $csv ]]; then
            printf '%s,%d,%s,%s\n' "$(basename "$image")" "$((i + 1))" "$label_a" "$last_a" >> "$csv"
            printf '%s,%d,%s,%s\n' "$(basename "$image")" "$((i + 1))" "$label_b" "$last_b" >> "$csv"
        fi
    done

    printf '%s\n' "${res_a[@]}" | summarize "$label_a"
    printf '%s\n' "${res_b[@]}" | summarize "$label_b"
done

[[ -n $csv ]] && printf '\nraw results: %s\n' "$csv"
exit 0
