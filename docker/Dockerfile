# syntax=docker/dockerfile:1
# iphone-control 镜像：一个 Rust 二进制 + Debian 的 usbmuxd。
# 所有版本锁在 ARG 里；Rust 依赖由 Cargo.lock 锁定（--locked）。

ARG DEBIAN_IMAGE=debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
ARG RUST_IMAGE=rust:1-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137

# ---- 编译 ----
FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p iphone-control \
 && cp target/release/iphone-control /iphone-control

# ---- WebDriverAgent：Appium 官方构建的未签名真机包，重新打成 IPA ----
# 计划改为本项目在 macOS CI 上自己编译（docs/roadmap.md 阶段 7）
FROM ${DEBIAN_IMAGE} AS wda
ARG WDA_VERSION=v16.15.3
ARG WDA_SHA256=2176793538d902f6b1e61291e9997f68d306541f222ea715d256a7c6ce9d4fc5
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl unzip zip \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL -o /tmp/wda.zip \
      "https://github.com/appium/WebDriverAgent/releases/download/${WDA_VERSION}/WebDriverAgentRunner-Runner.zip" \
 && echo "${WDA_SHA256}  /tmp/wda.zip" | sha256sum -c - \
 && mkdir -p /w/Payload /out/licenses/WebDriverAgent && cd /w/Payload && unzip -q /tmp/wda.zip \
 && cd /w && zip -qry /out/WDA-unsigned.ipa Payload \
 && curl -fsSL -o /out/licenses/WebDriverAgent/LICENSE \
      "https://raw.githubusercontent.com/appium/WebDriverAgent/${WDA_VERSION}/LICENSE"

# ---- 运行镜像 ----
FROM ${DEBIAN_IMAGE}
# usbmuxd 用 Debian 的包（GPL-3.0，独立进程运行；源码可从 Debian 获取）
RUN apt-get update \
 && apt-get install -y --no-install-recommends usbmuxd ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /iphone-control /usr/local/bin/iphone-control
COPY --from=wda /out/WDA-unsigned.ipa /usr/share/iphone-control/WDA-unsigned.ipa
COPY --from=wda /out/licenses/ /usr/share/licenses/
COPY LICENSE THIRD_PARTY.md /usr/share/doc/iphone-control/
COPY vendor/plume_core/LICENSE /usr/share/licenses/plume_core/LICENSE
COPY vendor/omnisette/LICENSE /usr/share/licenses/omnisette/LICENSE
COPY vendor/apple-codesign/LICENSE /usr/share/licenses/apple-codesign/LICENSE
# usbmuxd 把配对记录写在 /var/lib/lockdown，指到数据目录里，删掉重建容器也不丢
RUN rm -rf /var/lib/lockdown && ln -s /var/lib/iphone-control/lockdown /var/lib/lockdown
ENV ICTL_DATA_DIR=/var/lib/iphone-control
LABEL org.opencontainers.image.title="iphone-control" \
      org.opencontainers.image.description="Drive a real iPhone from Linux: WebDriverAgent + CoreDevice HID + MCP, single Rust binary" \
      org.opencontainers.image.licenses="MIT AND MPL-2.0"
ENTRYPOINT ["iphone-control"]
CMD ["daemon"]
