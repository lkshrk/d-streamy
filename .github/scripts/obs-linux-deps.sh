#!/usr/bin/env bash
# Usage: APT_ARCHIVES=dir obs-linux-deps.sh [EXTRA_PACKAGE...]
set -euo pipefail

exec "$(dirname "$0")/apt-install.sh" \
  build-essential cmake ninja-build pkg-config git curl jq libglib2.0-dev \
  extra-cmake-modules libcurl4-openssl-dev libavcodec-dev libavdevice-dev libavfilter-dev \
  libavformat-dev libavutil-dev libswresample-dev libswscale-dev libjansson-dev \
  libx11-xcb-dev libgles2-mesa-dev libwayland-dev libpipewire-0.3-dev libpulse-dev \
  libx264-dev libmbedtls-dev libgl1-mesa-dev uthash-dev libsimde-dev libx11-dev \
  libxcb-randr0-dev libxcb-shm0-dev libxcb-xinerama0-dev libxcb-composite0-dev \
  libxinerama-dev libxcb1-dev libxcb-xfixes0-dev libxss-dev libglvnd-dev libxkbcommon-dev \
  libxcomposite-dev libxdamage-dev libasound2-dev libfontconfig-dev libfreetype6-dev \
  libspeexdsp-dev libudev-dev libv4l-dev libva-dev libpci-dev libdrm-dev \
  nlohmann-json3-dev libwebsocketpp-dev libasio-dev libqrcodegencpp-dev qt6-base-dev \
  libqt6svg6-dev qt6-base-private-dev \
  libclang-dev libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  "$@"
