FROM ubuntu:24.04

ARG DEBIAN_FRONTEND=noninteractive
ENV TZ=Etc/UTC
ENV CARGO_HOME=/cargo RUSTUP_HOME=/rustup
RUN apt-get update \
	&& apt-get install -y xwayland libxcb1 clang libxcb-cursor0 libxcb-cursor-dev curl pkg-config libegl1 \
	&& rm -r /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
	&& . "/cargo/env" \
	&& rustup toolchain install stable \
	&& rustup default stable
RUN mkdir /run/xwls-test
ENV PATH="/cargo/bin:$PATH" XDG_RUNTIME_DIR="/run/xwls-test"
