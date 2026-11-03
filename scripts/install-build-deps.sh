#!/usr/bin/env bash
# Install the native build dependencies needed to compile ChirpStack in this
# sandbox, using ONLY the hosts the platform allows:
#   registry.npmjs.org, pypi.org / files.pythonhosted.org,
#   github.com / codeload.github.com / api.github.com
#
# apt, static.rust-lang.org and crates.io are blocked here, so:
#   protoc      -> PyPI wheel grpcio-tools (protoc 35.1) behind a tiny wrapper
#   cmake       -> PyPI wheel cmake (rdkafka-sys builds librdkafka with cmake)
#   libpq       -> shared library bundled in PyPI wheel psycopg2-binary
#   pg_config   -> small shim pointing at the libpq above
#   pkg-config  -> real pkgconf 3.0.10 built from its GitHub source with
#                  meson/ninja (both PyPI wheels)
#   ui/build    -> placeholder index.html (rust-embed requires the folder to
#                  exist; run `make build-ui` on a full dev machine for the
#                  real React UI)
#
# It also writes an isolated CARGO_HOME config that replaces crates.io with the
# vendored sources produced by scripts/fetch-vendor-crates.sh.
#
# Nothing here is committed: all binaries live in ~/.cache (excluded from
# workspace snapshots) with symlinks in /usr/local/bin.

set -euo pipefail

CACHE="${HOME}/.cache"
PYENV="${CACHE}/pytools"
CARGO_HOME_DIR="${CACHE}/cargo-home"
VENDOR_DIR="${CACHE}/vendor"
SYSLIBS="${CACHE}/syslibs"
BIN_DIR="/usr/local/bin"

echo "==> Python venv with the wheels we need"
if [ ! -x "${PYENV}/bin/python" ]; then
  python3 -m venv "${PYENV}"
fi
"${PYENV}/bin/pip" install --quiet --disable-pip-version-check \
  tree-sitter tree-sitter-rust grpcio-tools cmake psycopg2-binary meson ninja

echo "==> protoc (grpcio-tools) and cmake"
cat > "${BIN_DIR}/protoc" <<SH
#!/bin/sh
# protoc provided by the PyPI grpcio-tools wheel (apt / GitHub releases blocked).
exec "\${HOME}/.cache/pytools/bin/python" -m grpc_tools.protoc "\$@"
SH
chmod +x "${BIN_DIR}/protoc"
CMAKE_BIN="$(find "${PYENV}" -type f -name cmake -path '*/bin/*' | head -1)"
ln -sf "${CMAKE_BIN}" "${BIN_DIR}/cmake"
protoc --version
cmake --version | head -1

echo "==> libpq + pg_config shim"
mkdir -p /usr/local/lib "${SYSLIBS}/lib" "${SYSLIBS}/include"
LPQ="$(find "${PYENV}" -name 'libpq*.so*' | head -1)"
cp "${LPQ}" /usr/local/lib/libpq.so.5
ln -sf /usr/local/lib/libpq.so.5 /usr/local/lib/libpq.so
ln -sf /usr/local/lib/libpq.so.5 "${SYSLIBS}/lib/libpq.so"
cat > "${BIN_DIR}/pg_config" <<SH
#!/bin/sh
# Minimal pg_config for crates probing libpq. ChirpStack v4.20 itself uses
# tokio-postgres, so libpq is only needed by optional/native integrations.
case "\$1" in
  --libdir)     echo /usr/local/lib ;;
  --includedir) echo "\${HOME}/.cache/syslibs/include" ;;
  --version)    echo 17.0 ;;
  --bindir)     echo /usr/local/bin ;;
  *)            echo "" ;;
esac
SH
chmod +x "${BIN_DIR}/pg_config"
sudo ldconfig 2>/dev/null || true

echo "==> pkgconf (real pkg-config) from GitHub source"
if [ ! -x "${CACHE}/pkgconf/bin/pkgconf" ]; then
  WORK="$(mktemp -d)"
  trap 'rm -rf "${WORK}"' EXIT
  TAG="$(gh api 'repos/pkgconf/pkgconf/tags?per_page=1' --jq '.[0].name' 2>/dev/null || echo master)"
  curl -sS -L --fail -o "${WORK}/pkgconf.tar.gz" \
    "https://codeload.github.com/pkgconf/pkgconf/tar.gz/refs/tags/${TAG}"
  tar xzf "${WORK}/pkgconf.tar.gz" -C "${WORK}" --strip-components=1
  export PATH="${PYENV}/bin:${PATH}"
  (cd "${WORK}" && meson setup build --prefix="${CACHE}/pkgconf" >/dev/null \
    && ninja -C build >/dev/null \
    && meson install -C build --no-rebuild >/dev/null)
fi
cp -a "${CACHE}"/pkgconf/lib/*/libpkgconf.so* /usr/local/lib/ 2>/dev/null || true
ln -sf "${CACHE}/pkgconf/bin/pkgconf" "${BIN_DIR}/pkg-config"
ln -sf "${CACHE}/pkgconf/bin/pkgconf" "${BIN_DIR}/pkgconf"
sudo ldconfig 2>/dev/null || true
mkdir -p "${CACHE}/pkgconf/lib/pkgconfig"
cat > "${CACHE}/pkgconf/lib/pkgconfig/libpq.pc" <<PC
prefix=/usr/local
libdir=\${prefix}/lib
includedir=${SYSLIBS}/include

Name: libpq
Description: PostgreSQL client library (PyPI psycopg2-binary wheel)
Version: 17.0
Libs: -L\${libdir} -lpq
Cflags: -I\${includedir}
PC
pkg-config --version
PKG_CONFIG_PATH="${CACHE}/pkgconf/lib/pkgconfig" pkg-config --libs libpq

echo "==> ui/build placeholder for rust-embed"
if [ ! -f ui/build/index.html ]; then
  mkdir -p ui/build
  cat > ui/build/index.html <<'HTML'
<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>ChirpStack (dev placeholder)</title></head>
<body><p>UI build placeholder. Run <code>make build-ui</code> for the real UI.</p></body></html>
HTML
fi

echo "==> cargo config (vendored sources, offline)"
mkdir -p "${CARGO_HOME_DIR}"
cat > "${CARGO_HOME_DIR}/config.toml" <<TOML
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "${VENDOR_DIR}"

[net]
offline = true
TOML

echo "==> Done. Export CARGO_HOME=${CARGO_HOME_DIR} before building."
echo "    protoc     : $(protoc --version)"
echo "    cmake      : $(cmake --version | head -1)"
echo "    pkg-config : $(pkg-config --version)"
echo "    pg_config  : $(pg_config --version)"
echo "    vendor dir : ${VENDOR_DIR} (run scripts/fetch-vendor-crates.sh first if empty)"
