#!/usr/bin/env bash
# Downloads ONNX Runtime (used for voice-activity detection, on CPU) into vendor/onnxruntime.
# Needed once per machine. The Docker image does this by itself.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${ONNXRUNTIME_VERSION:-1.23.2}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  NAME="onnxruntime-linux-x64-$VERSION" ;;
  Linux-aarch64) NAME="onnxruntime-linux-aarch64-$VERSION" ;;
  Darwin-arm64)  NAME="onnxruntime-osx-arm64-$VERSION" ;;
  Darwin-x86_64) NAME="onnxruntime-osx-x86_64-$VERSION" ;;
  *) echo "No ready-made ONNX Runtime for $(uname -s) $(uname -m). Install it yourself and set ORT_DYLIB_PATH to the library file." >&2; exit 1 ;;
esac

if ls vendor/onnxruntime/lib/libonnxruntime.* >/dev/null 2>&1; then
  echo "ONNX Runtime is already in vendor/onnxruntime."
  exit 0
fi

mkdir -p vendor
echo "Downloading $NAME ..."
curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v$VERSION/$NAME.tgz" | tar -xz -C vendor
rm -rf vendor/onnxruntime
mv "vendor/$NAME" vendor/onnxruntime
echo "Done: $(ls vendor/onnxruntime/lib | head -3 | tr '\n' ' ')"
