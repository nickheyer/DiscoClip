#!/bin/sh
# Checks a built image: the server answers, the tools it carries are the ones the
# transcoder needs, and a stop ends it cleanly.
#
#   packaging/smoke-test.sh <image> <cpu|gpu>
#
# The gpu check lists the hardware encoders and the filters the image was built for, and
# renders a test picture through libplacebo on a Vulkan device ffmpeg opens, which is
# Mesa's CPU driver on a machine without a GPU: the Vulkan loader, the shader compiler and
# libplacebo work in the image.

set -eu

image=${1:?image}
variant=${2:?cpu or gpu}
name="discoclip-smoke-$$"

say() { printf '\n== %s\n' "$*"; }
fail() { printf 'smoke test failed: %s\n' "$*" >&2; exit 1; }

say "version"
version=$(docker run --rm "$image" --version)
echo "$version"
case "$version" in
    "discoclip "*) ;;
    *) fail "unexpected --version output" ;;
esac

if [ "$variant" = gpu ]; then
    arch=$(docker run --rm --entrypoint dpkg "$image" --print-architecture)
    say "ffmpeg ($arch)"
    docker run --rm --entrypoint ffmpeg "$image" -version | head -1
    docker run --rm --entrypoint ffprobe "$image" -version | head -1

    encoders=$(docker run --rm --entrypoint ffmpeg "$image" -hide_banner -encoders)
    wanted="h264_nvenc hevc_nvenc av1_nvenc h264_vaapi hevc_vaapi av1_vaapi vp9_vaapi vp8_vaapi libx264 libx265 libsvtav1 libaom-av1 libvpx-vp9 libvpx aac libopus libmp3lame libvorbis flac"
    if [ "$arch" = amd64 ]; then
        wanted="$wanted h264_qsv hevc_qsv av1_qsv vp9_qsv"
    fi
    for encoder in $wanted; do
        echo "$encoders" | awk -v e="$encoder" '$2 == e { found = 1 } END { exit !found }' \
            || fail "encoder $encoder is missing"
    done
    echo "encoders: $wanted"

    filters=$(docker run --rm --entrypoint ffmpeg "$image" -hide_banner -filters)
    for filter in libplacebo zscale tonemap subtitles v360 bwdif stereo3d hwupload scale_vaapi scale_cuda showwaves; do
        echo "$filters" | awk -v f="$filter" '$2 == f { found = 1 } END { exit !found }' \
            || fail "filter $filter is missing"
    done
    echo "filters: libplacebo zscale tonemap subtitles v360 bwdif stereo3d hwupload scale_vaapi scale_cuda showwaves"

    say "vulkan"
    docker run --rm --entrypoint vulkaninfo "$image" --summary | grep -i -E 'deviceName|driverName' \
        || fail "vulkaninfo lists no device"
    docker run --rm --entrypoint ffmpeg "$image" -hide_banner -loglevel error \
        -init_hw_device vulkan=vk -filter_hw_device vk \
        -f lavfi -i testsrc2=s=128x72:r=10:d=0.3 \
        -vf 'libplacebo=tonemapping=auto:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p' \
        -f null - || fail "libplacebo could not render on the Vulkan device ffmpeg opened"
    echo "libplacebo rendered on Vulkan"

    say "va-api"
    docker run --rm --entrypoint sh "$image" -c 'ls /usr/lib/*/dri/*_drv_video.so' \
        || fail "no VA-API drivers are installed"
fi

say "start"
docker rm -f "$name" >/dev/null 2>&1 || true
docker run -d --name "$name" "$image" >/dev/null
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT

deadline=$(( $(date +%s) + 180 ))
until body=$(docker exec "$name" curl -fsS http://127.0.0.1:8080/healthz 2>/dev/null); do
    if [ "$(date +%s)" -ge "$deadline" ]; then
        docker logs "$name" | tail -50
        fail "/healthz did not answer 200 within 180 s"
    fi
    if [ "$(docker inspect -f '{{.State.Running}}' "$name")" != true ]; then
        docker logs "$name" | tail -50
        fail "the container exited"
    fi
    sleep 2
done
echo "healthz: $body"
docker logs "$name" 2>&1 | grep -E 'encoding through|ffmpeg version' || true

say "stop"
docker stop -t 75 "$name" >/dev/null
code=$(docker wait "$name")
[ "$code" = 0 ] || { docker logs "$name" | tail -50; fail "exit status $code after SIGTERM"; }
echo "stopped cleanly"
