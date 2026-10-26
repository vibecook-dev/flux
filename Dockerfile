# flux container image: flux-server, its web client and jellyfin-ffmpeg on
# Debian 13 (trixie). Nothing is compiled here; the image is assembled from
# the Linux release packages (docker/README.md):
#
#   # from a published release
#   docker buildx build --build-arg FLUX_VERSION=0.1.0-alpha.1 \
#     --platform linux/amd64,linux/arm64 -t flux .
#   # from local packages (what CI does)
#   scripts/package/docker-context.sh dist docker-dist
#   docker buildx build --build-context dist=docker-dist -t flux .

# The base image, debian:trixie-slim pinned by digest, is named in both FROM
# lines below, not in an ARG: Dependabot only reads image references written
# out in FROM lines, and it updates both together.

# ---------------------------------------------------------------------------
# download: a published release package per architecture, checked against
# the release's SHA256SUMS. Runs on the build machine's platform.
FROM --platform=$BUILDPLATFORM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS download
ARG TARGETARCH
ARG FLUX_VERSION
ARG FLUX_REPOSITORY=vibecook-dev/flux
SHELL ["/bin/bash", "-o", "pipefail", "-c"]
# hadolint ignore=DL3008
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /tmp/flux
RUN if [ -z "${FLUX_VERSION}" ]; then \
      echo "Set --build-arg FLUX_VERSION=<release> or pass --build-context dist=<dir> (docker/README.md)." >&2; \
      exit 1; \
    fi; \
    case "${TARGETARCH}" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    pkg="flux-${FLUX_VERSION}-${target}.tar.gz"; \
    base="https://github.com/${FLUX_REPOSITORY}/releases/download/v${FLUX_VERSION}"; \
    curl -fsSL --retry 3 -O "${base}/${pkg}"; \
    curl -fsSL --retry 3 -O "${base}/SHA256SUMS"; \
    grep -E "  ${pkg}\$" SHA256SUMS | sha256sum -c -; \
    mkdir -p "/out/${TARGETARCH}"; \
    tar -xzf "${pkg}" --strip-components=1 -C "/out/${TARGETARCH}"

# ---------------------------------------------------------------------------
# dist: /<amd64|arm64>/ holding an extracted package (bin/, share/flux/web/,
# LICENSE, THIRD_PARTY_LICENSES). `--build-context dist=<dir>` replaces it.
FROM scratch AS dist
COPY --from=download /out/ /

# ---------------------------------------------------------------------------
FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f
ARG TARGETARCH
# jellyfin-ffmpeg: https://github.com/jellyfin/jellyfin-ffmpeg/releases
# Bump the version and both .deb checksums together (docs/RELEASING.md).
ARG JELLYFIN_FFMPEG_VERSION=8.1.3-1
ARG JELLYFIN_FFMPEG_SHA256_AMD64=fbef9f81a53e175194e3f67832618a86111f7bd37197df21a5d748c34289f9c0
ARG JELLYFIN_FFMPEG_SHA256_ARM64=4af743bd776eda082d40c851a4201e51a77a258145dd6ebe164dade39b35b2d0
SHELL ["/bin/bash", "-o", "pipefail", "-c"]

# ffmpeg and ffprobe land in /usr/lib/jellyfin-ffmpeg, with the VAAPI/QSV
# drivers they need; the rest of their libraries come from Debian.
# hadolint ignore=DL3008
RUN case "${TARGETARCH}" in \
      amd64) sha="${JELLYFIN_FFMPEG_SHA256_AMD64}" ;; \
      arm64) sha="${JELLYFIN_FFMPEG_SHA256_ARM64}" ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && major="${JELLYFIN_FFMPEG_VERSION%%.*}" \
 && deb="jellyfin-ffmpeg${major}_${JELLYFIN_FFMPEG_VERSION}-trixie_${TARGETARCH}.deb" \
 && apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tini \
 && curl -fsSL --retry 3 -o "/tmp/${deb}" \
      "https://github.com/jellyfin/jellyfin-ffmpeg/releases/download/v${JELLYFIN_FFMPEG_VERSION}/${deb}" \
 && echo "${sha}  /tmp/${deb}" | sha256sum -c - \
 && apt-get install -y --no-install-recommends "/tmp/${deb}" \
 && mkdir -p /usr/share/licenses/jellyfin-ffmpeg \
 && dpkg-deb --fsys-tarfile "/tmp/${deb}" \
      | tar -xO "./usr/share/doc/jellyfin-ffmpeg${major}/copyright" \
      > /usr/share/licenses/jellyfin-ffmpeg/copyright \
 && rm -f "/tmp/${deb}" \
 && rm -rf /var/lib/apt/lists/* \
 && /usr/lib/jellyfin-ffmpeg/ffmpeg -hide_banner -version | sed -n 1p

COPY docker/SOURCE-OFFER.md /usr/share/licenses/jellyfin-ffmpeg/SOURCE-OFFER.md
RUN sed -i "s/@JELLYFIN_FFMPEG_VERSION@/${JELLYFIN_FFMPEG_VERSION}/g" \
      /usr/share/licenses/jellyfin-ffmpeg/SOURCE-OFFER.md

COPY --from=dist /${TARGETARCH}/ /opt/flux/

RUN groupadd --gid 1000 flux \
 && useradd --uid 1000 --gid flux --home-dir /config --no-create-home \
      --shell /usr/sbin/nologin flux \
 && mkdir -p /config /media \
 && chown flux:flux /config \
 && ln -s /opt/flux/bin/flux-server /usr/local/bin/flux-server \
 && if [ -x /opt/flux/bin/flux ]; then ln -s /opt/flux/bin/flux /usr/local/bin/flux; fi \
 && flux-server --version

# In a container the listener binds every interface. Only callers on the
# container's own loopback (docker exec, the health check) are the Owner;
# everyone else, the host included, is an anonymous LAN caller. Browsers
# must reach flux by a name it allows: localhost, the container's own name
# and addresses, or one listed in FLUX_ALLOW_HOSTS. See docs/INSTALL.md.
ENV FLUX_DATA_DIR=/config \
    FLUX_BIND=0.0.0.0 \
    FLUX_PORT=8096 \
    FLUX_WEB_DIR=/opt/flux/share/flux/web \
    FLUX_FFMPEG=/usr/lib/jellyfin-ffmpeg/ffmpeg \
    FLUX_FFPROBE=/usr/lib/jellyfin-ffmpeg/ffprobe \
    HOME=/config

VOLUME ["/config", "/media"]
EXPOSE 8096
USER 1000:1000
WORKDIR /config

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
  CMD ["sh", "-c", "curl -fsS -o /dev/null \"http://127.0.0.1:${FLUX_PORT:-8096}/api/v1/server\" || exit 1"]

# tini reaps the ffmpeg processes flux starts and forwards SIGTERM to it.
ENTRYPOINT ["/usr/bin/tini", "--", "/opt/flux/bin/flux-server"]

LABEL org.opencontainers.image.title="flux" \
      org.opencontainers.image.description="flux media server: browse, play and stream your movie and TV library" \
      org.opencontainers.image.source="https://github.com/vibecook-dev/flux" \
      org.opencontainers.image.documentation="https://github.com/vibecook-dev/flux/blob/main/docs/INSTALL.md" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.vendor="vibecook-dev" \
      dev.vibecook.flux.ffmpeg="jellyfin-ffmpeg ${JELLYFIN_FFMPEG_VERSION}, GPL-3.0-or-later; licence and source in /usr/share/licenses/jellyfin-ffmpeg"
